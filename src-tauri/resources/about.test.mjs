import { readFileSync } from "node:fs";
import { strict as assert } from "node:assert";
import { test } from "node:test";
import { runInNewContext } from "node:vm";

const html = readFileSync(new URL("./about.html", import.meta.url), "utf8");
const i18n = readFileSync(new URL("./i18n.js", import.meta.url), "utf8");
const script = readFileSync(new URL("./about.js", import.meta.url), "utf8");
const english = JSON.parse(
  readFileSync(new URL("./translations/en.json", import.meta.url), "utf8")
);
const releases = "https://github.com/music-assistant/desktop-app/releases";

function setup(invoke) {
  const elements = {
    version: { textContent: "Version" },
    "release-notes": {
      href: releases,
      textContent: "Release notes",
      dataset: { i18n: "desktop.about.release_notes" },
    },
  };
  const document = {
    title: "About Music Assistant",
    documentElement: { classList: { toggle() {} }, lang: "en" },
    getElementById(id) {
      return elements[id];
    },
    querySelectorAll(selector) {
      return selector === "[data-i18n]" ? [elements["release-notes"]] : [];
    },
  };
  const errors = [];
  const window = {
    __TAURI__: { core: { invoke } },
    matchMedia: () => ({ matches: false, addEventListener() {} }),
  };
  const context = {
    window,
    document,
    console: { error: (...args) => errors.push(args) },
    redactDiagnostic: () => "redacted",
  };
  runInNewContext(i18n, context);
  Object.assign(context, {
    initI18n: window.initI18n,
    applyTranslations: window.applyTranslations,
    t: window.t,
  });
  runInNewContext(script, context);
  return { elements, document, errors, context };
}

async function settle() {
  for (let i = 0; i < 5; i++) await Promise.resolve();
}

test("About identifies the companion app rather than the server", () => {
  assert.match(
    html,
    /<h1 data-i18n="desktop\.about\.app_name">Music Assistant Companion App<\/h1>/
  );
  assert.equal(english.desktop.about.app_name, "Music Assistant Companion App");
  assert.match(html, /<title>About Music Assistant Companion App<\/title>/);
});

test("About link is accessible and defaults to release history", () => {
  assert.match(html, /<div class="version" id="version">Version<\/div>\s*<a\b/);
  assert.match(html, /id="release-notes"\s+data-i18n="desktop\.about\.release_notes"/);
  assert.match(
    html,
    /href="https:\/\/github\.com\/music-assistant\/desktop-app\/releases"\s+target="_blank"\s+rel="noopener noreferrer"/
  );
  assert.match(html, />Release notes<\/a/);
  assert.match(html, /<script src="about\.js"><\/script>/);
  assert.equal(english.desktop.about.release_notes, "Release notes");
});

test("only released numeric versions form tag URLs", () => {
  const { context } = setup();
  const url = (version) => runInNewContext("releaseNotesUrl(version)", { ...context, version });
  assert.equal(url("0.7.2"), `${releases}/tag/0.7.2`);
  for (const version of [
    "0.0.0",
    "",
    "unknown",
    "dev",
    "0.7.2-dev",
    "v0.7.2",
    " 0.7.2 ",
    "0.7.2/evil",
    "00.7.2",
    "0.07.2",
    "<img src=x onerror=alert(1)>",
    null,
    undefined,
    72,
  ]) {
    assert.equal(url(version), releases, String(version));
  }
});

test("About uses running app version and English fallback for untranslated locales", async () => {
  const calls = [];
  const { elements, document, errors } = setup(async (command) => {
    calls.push(command);
    if (command === "get_i18n_bundle")
      return {
        locale: "nl",
        messages: { desktop: { about: { version: "Versie {0}" } } },
        fallback_messages: english,
      };
    if (command === "get_app_version") return "0.7.2";
    throw new Error("Unexpected command");
  });
  await settle();
  assert.deepEqual(calls, ["get_i18n_bundle", "get_app_version"]);
  assert.equal(elements.version.textContent, "Versie 0.7.2");
  assert.equal(elements["release-notes"].textContent, "Release notes");
  assert.equal(elements["release-notes"].href, `${releases}/tag/0.7.2`);
  assert.equal(document.documentElement.lang, "nl");
  assert.equal(document.title, "About Music Assistant Companion App");
  assert.equal(errors.length, 0);
});

test("debug versions and failed version lookups retain release history", async () => {
  for (const version of ["0.0.0", "0.7.2-preview", undefined]) {
    const { elements } = setup(async (command) =>
      command === "get_i18n_bundle" ? { messages: english, fallback_messages: english } : version
    );
    await settle();
    assert.equal(elements["release-notes"].href, releases);
  }
  const { elements, errors } = setup(async (command) => {
    if (command === "get_i18n_bundle") return { messages: english, fallback_messages: english };
    throw new Error("version failed");
  });
  await settle();
  assert.equal(elements["release-notes"].href, releases);
  assert.equal(errors.length, 1);
  assert.equal(errors[0][1], "redacted");
});
