import { readFileSync } from "node:fs";
import { strict as assert } from "node:assert";
import { test } from "node:test";
import { runInNewContext } from "node:vm";

const html = readFileSync(new URL("./about.html", import.meta.url), "utf8");
const i18n = readFileSync(new URL("./i18n.js", import.meta.url), "utf8");
const script = readFileSync(new URL("./about.js", import.meta.url), "utf8");
const translations = Object.fromEntries(
  ["en", "de", "nl"].map((locale) => [
    locale,
    JSON.parse(readFileSync(new URL(`./translations/${locale}.json`, import.meta.url), "utf8")),
  ])
);
const english = translations.en;
const releases = "https://github.com/music-assistant/desktop-app/releases";
const bundle = (locale = "en") => ({
  locale,
  messages: translations[locale],
  fallback_messages: english,
});
const invokeVersion =
  (locale = "en", version = "0.7.2") =>
  async (command) =>
    command === "get_i18n_bundle" ? bundle(locale) : version;

function setup(invoke = invokeVersion(), layout = {}) {
  const sizes = [];
  const positions = [];
  const clones = [];
  const position = { x: layout.initialX ?? 100, y: layout.initialY ?? 100 };
  const scale = layout.scaleFactor ?? 1;
  let nativeSize = {
    width: layout.innerWidth ?? 460,
    height: (layout.innerHeight ?? 350) + (layout.nativeInnerHeightOffset ?? 0),
  };
  let onContentResize;
  let onWindowResize;
  const elements = {
    version: { textContent: "Version" },
    "release-notes": {
      href: releases,
      textContent: "Release notes",
      dataset: { i18n: "desktop.about.release_notes" },
    },
    heading: {
      textContent: "Music Assistant Companion App",
      dataset: { i18n: "desktop.about.app_name" },
    },
  };
  const main = {
    getBoundingClientRect() {
      return { height: layout.actualHeight ?? layout.contentHeight ?? 286 };
    },
    cloneNode() {
      const clone = {
        style: {},
        removed: false,
        idsRemoved: [],
        removeAttribute(attribute) {
          this.idsRemoved.push(attribute);
        },
        setAttribute(attribute, value) {
          this[attribute] = value;
        },
        querySelectorAll(selector) {
          assert.equal(selector, "[id]");
          return [
            { removeAttribute: (name) => this.idsRemoved.push(`version:${name}`) },
            { removeAttribute: (name) => this.idsRemoved.push(`release-notes:${name}`) },
          ];
        },
        getBoundingClientRect() {
          const natural = layout.contentWidth ?? 380;
          const bounded = parseFloat(this.style.width);
          return {
            width: Number.isNaN(bounded) ? natural : bounded,
            height:
              (layout.contentHeight ?? 286) +
              (Number.isNaN(bounded)
                ? 0
                : (layout.wrappedHeight ?? 0) * Math.max(0, Math.ceil(natural / bounded) - 1)),
          };
        },
        remove() {
          this.removed = true;
        },
      };
      clones.push(clone);
      return clone;
    },
  };
  const document = {
    title: "About Music Assistant",
    documentElement: { classList: { toggle() {} }, lang: "en" },
    body: {
      appendChild(node) {
        assert.equal(node["aria-hidden"], "true");
        assert.equal(node.inert, true);
      },
    },
    getElementById(id) {
      return elements[id];
    },
    querySelectorAll(selector) {
      return selector === "[data-i18n]" ? [elements.heading, elements["release-notes"]] : [];
    },
    querySelector(selector) {
      return selector === "main" ? main : null;
    },
    fonts: { ready: Promise.resolve() },
  };
  const errors = [];
  const window = {
    innerWidth: layout.innerWidth ?? 460,
    innerHeight: layout.innerHeight ?? 350,
    screen: {
      availWidth: "screenWidth" in layout ? layout.screenWidth : 800,
      availHeight: layout.screenHeight,
    },
    matchMedia: () => ({ matches: false, addEventListener() {} }),
    addEventListener: (name, listener) => {
      if (name === "resize") onWindowResize = listener;
    },
    getComputedStyle: () => ({
      paddingTop: "24px",
      paddingBottom: "24px",
      paddingLeft: "24px",
      paddingRight: "24px",
    }),
  };
  const currentWindow = {
    scaleFactor: async () => scale,
    innerSize: async () => ({
      width: (layout.delayedViewport ? nativeSize.width : window.innerWidth) * scale,
      height:
        (layout.delayedViewport
          ? nativeSize.height
          : window.innerHeight + (layout.nativeInnerHeightOffset ?? 0)) * scale,
    }),
    outerSize: async () => ({
      width:
        (layout.delayedViewport ? nativeSize.width : window.innerWidth) * scale +
        (layout.frameWidth ?? 0),
      height:
        (layout.delayedViewport ? nativeSize.height : window.innerHeight) * scale +
        (layout.frameHeight ?? 40),
    }),
    outerPosition: async () => ({ ...position }),
    setSize: async (size) => {
      sizes.push(size);
      if (layout.delayedViewport) nativeSize = { width: size.width, height: size.height };
      if (!layout.delayedViewport) {
        window.innerWidth = size.width;
        window.innerHeight = size.height;
      }
    },
    setPosition: async (next) => {
      positions.push(next);
      Object.assign(position, next);
    },
  };
  window.__TAURI__ = {
    core: { invoke },
    window: {
      getCurrentWindow: () => currentWindow,
      currentMonitor: async () =>
        layout.noMonitor
          ? null
          : {
              workArea: {
                position: { x: layout.workX ?? 0, y: layout.workY ?? 0 },
                size: { width: layout.workWidth ?? 1200, height: layout.workHeight ?? 900 },
              },
              scaleFactor: scale,
            },
      PhysicalPosition: class PhysicalPosition {
        constructor(x, y) {
          Object.assign(this, { x, y });
        }
      },
      LogicalSize: class LogicalSize {
        constructor(width, height) {
          Object.assign(this, { width, height });
        }
      },
    },
  };
  const context = {
    window,
    document,
    ResizeObserver: class ResizeObserver {
      constructor(callback) {
        onContentResize = callback;
      }
      observe(node) {
        assert.equal(node, main);
      }
    },
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
  return {
    elements,
    document,
    errors,
    context,
    sizes,
    positions,
    clones,
    onContentResize: () => onContentResize(),
    onWindowResize: (width, height) => {
      window.innerWidth = width;
      window.innerHeight = height;
      onWindowResize();
    },
  };
}

async function settle() {
  for (let i = 0; i < 30; i++) await Promise.resolve();
}

const pairs = (values) => values.map(({ width, height }) => [width, height]);
const coordinates = (values) => values.map(({ x, y }) => [x, y]);

test("About identifies the companion app and has an accessible release-history link", () => {
  assert.match(
    html,
    /<h1 data-i18n="desktop\.about\.app_name">Music Assistant Companion App<\/h1>/
  );
  assert.equal(english.desktop.about.app_name, "Music Assistant Companion App");
  assert.match(html, /<title>About Music Assistant Companion App<\/title>/);
  assert.match(html, /<div class="version" id="version">Version<\/div>\s*<a\b/);
  assert.match(html, /id="release-notes"\s+data-i18n="desktop\.about\.release_notes"/);
  assert.match(
    html,
    /href="https:\/\/github\.com\/music-assistant\/desktop-app\/releases"\s+target="_blank"\s+rel="noopener noreferrer"/
  );
  assert.match(html, />Release notes<\/a/);
  assert.equal(english.desktop.about.release_notes, "Release notes");
  assert.doesNotMatch(html, /max-width:\s*320px/);
  assert.match(html, /overflow-wrap:\s*anywhere/);
  assert.match(html, /align-items:\s*safe center/);
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

test("About uses app version and real German and Dutch release-note translations", async () => {
  for (const locale of ["de", "nl"]) {
    const calls = [];
    const { elements, document, errors } = setup(async (command) => {
      calls.push(command);
      return invokeVersion(locale)(command);
    });
    await settle();
    assert.deepEqual(calls, ["get_i18n_bundle", "get_app_version"]);
    assert.equal(
      elements.version.textContent,
      translations[locale].desktop.about.version.replace("{0}", "0.7.2")
    );
    assert.equal(
      elements["release-notes"].textContent,
      translations[locale].desktop.about.release_notes
    );
    assert.equal(elements["release-notes"].href, `${releases}/tag/0.7.2`);
    assert.equal(document.documentElement.lang, locale);
    assert.equal(errors.length, 0);
  }
});

test("About measures natural width, then refits height after content changes without feedback", async () => {
  const layout = { contentWidth: 380, contentHeight: 340 };
  const { sizes, clones, onContentResize } = setup(invokeVersion(), layout);
  await settle();
  assert.deepEqual(pairs(sizes), [[436, 396]]);
  assert.equal(clones[0].removed, true);
  assert.deepEqual(clones[0].idsRemoved, ["id", "version:id", "release-notes:id"]);
  onContentResize();
  await settle();
  assert.equal(sizes.length, 1, "unchanged measurements must not resize again");
  layout.contentHeight = 400;
  layout.contentWidth = 520;
  onContentResize();
  await settle();
  assert.deepEqual(pairs(sizes), [
    [436, 396],
    [576, 456],
  ]);
});

test("About corrects a wrapped title after the native viewport reaches its new width", async () => {
  const layout = {
    contentWidth: 323,
    contentHeight: 222,
    actualHeight: 222,
    delayedViewport: true,
  };
  const { sizes, onWindowResize } = setup(invokeVersion(), layout);
  await settle();
  assert.deepEqual(pairs(sizes), [[379, 278]]);
  layout.actualHeight = 252;
  onWindowResize(379, 278);
  await settle();
  assert.deepEqual(pairs(sizes), [
    [379, 278],
    [379, 308],
  ]);
  onWindowResize(379, 308);
  await settle();
  assert.equal(sizes.length, 2, "the correction must settle without a resize loop");
});

test("About accounts for native height outside the actual WebView viewport", async () => {
  const { sizes, onWindowResize } = setup(invokeVersion(), {
    contentWidth: 323,
    contentHeight: 222,
    nativeInnerHeightOffset: 28,
    delayedViewport: true,
  });
  await settle();
  assert.deepEqual(pairs(sizes), [[379, 306]]);
  onWindowResize(379, 278);
  await settle();
  assert.equal(sizes.length, 1, "actual viewport height must settle after the native resize");
});

test("About wraps wide content inside monitor width and caps its resulting height at 2x scale", async () => {
  const { sizes, positions } = setup(invokeVersion(), {
    contentWidth: 700,
    contentHeight: 340,
    wrappedHeight: 360,
    workWidth: 800,
    workHeight: 900,
    scaleFactor: 2,
    frameWidth: 20,
    initialX: 300,
  });
  await settle();
  assert.deepEqual(pairs(sizes), [[390, 430]]);
  assert.deepEqual(coordinates(positions), [[0, 0]]);
});

test("About clamps physical position for negative monitor origins at 2x scale", async () => {
  const { sizes, positions } = setup(invokeVersion(), {
    contentWidth: 380,
    contentHeight: 700,
    initialX: -700,
    initialY: -50,
    workX: -1200,
    workY: -100,
    workWidth: 1000,
    workHeight: 900,
    scaleFactor: 2,
  });
  await settle();
  assert.deepEqual(pairs(sizes), [[436, 430]]);
  assert.deepEqual(coordinates(positions), [[-1072, -100]]);
});

test("About caps both dimensions on a short screen without monitor information", async () => {
  const { sizes, positions } = setup(invokeVersion(), {
    noMonitor: true,
    contentWidth: 650,
    contentHeight: 400,
    screenWidth: 320,
    screenHeight: 260,
  });
  await settle();
  assert.deepEqual(pairs(sizes), [[320, 220]]);
  assert.equal(positions.length, 0);
});

test("About does not grow vertically without monitor position data", async () => {
  const { sizes, positions } = setup(invokeVersion(), {
    noMonitor: true,
    contentWidth: 650,
    contentHeight: 400,
    screenWidth: 320,
    screenHeight: 900,
  });
  await settle();
  assert.deepEqual(pairs(sizes), [[320, 350]]);
  assert.equal(positions.length, 0);
});

test("About uses viewport height as-is when screen height is unavailable", async () => {
  const { sizes, positions, onWindowResize } = setup(invokeVersion(), {
    noMonitor: true,
    scaleFactor: 2,
    contentWidth: 650,
    contentHeight: 400,
    screenWidth: 320,
    frameHeight: 40,
    delayedViewport: true,
  });
  await settle();
  assert.deepEqual(pairs(sizes), [[320, 350]]);
  onWindowResize(320, 350);
  await settle();
  assert.deepEqual(
    pairs(sizes),
    [[320, 350]],
    "the native resize must not shrink the viewport again"
  );
  assert.equal(positions.length, 0);
});

test("About does not subtract native frames again when both screen dimensions are unavailable", async () => {
  const { sizes, positions, onWindowResize } = setup(invokeVersion(), {
    noMonitor: true,
    scaleFactor: 2,
    contentWidth: 300,
    contentHeight: 250,
    screenWidth: undefined,
    screenHeight: undefined,
    frameWidth: 20,
    frameHeight: 80,
    nativeInnerHeightOffset: 28,
    delayedViewport: true,
  });
  await settle();
  assert.deepEqual(pairs(sizes), [[356, 334]]);
  onWindowResize(356, 306);
  await settle();
  onWindowResize(356, 306);
  await settle();
  assert.deepEqual(pairs(sizes), [[356, 334]], "repeat viewport reports must not resize again");
  assert.equal(positions.length, 0);
});

test("About ignores invalid available screen dimensions", async () => {
  for (const invalid of [0, -1, NaN]) {
    const { sizes, onWindowResize } = setup(invokeVersion(), {
      noMonitor: true,
      screenWidth: invalid,
      screenHeight: invalid,
      contentWidth: 300,
      contentHeight: 250,
      delayedViewport: true,
    });
    await settle();
    assert.deepEqual(pairs(sizes), [[356, 306]]);
    onWindowResize(356, 306);
    await settle();
    assert.deepEqual(pairs(sizes), [[356, 306]]);
  }
});

test("debug versions and failed version lookups retain release history", async () => {
  for (const version of ["0.0.0", "0.7.2-preview", undefined]) {
    const { elements } = setup(async (command) =>
      command === "get_i18n_bundle" ? bundle() : version
    );
    await settle();
    assert.equal(elements["release-notes"].href, releases);
  }
  const { elements, errors } = setup(async (command) => {
    if (command === "get_i18n_bundle") return bundle();
    throw new Error("version failed");
  });
  await settle();
  assert.equal(elements["release-notes"].href, releases);
  assert.equal(errors.length, 1);
  assert.equal(errors[0][1], "redacted");
});
