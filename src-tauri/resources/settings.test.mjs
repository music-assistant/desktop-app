import { readFileSync } from "node:fs";
import { strict as assert } from "node:assert";
import vm from "node:vm";
import { test } from "node:test";

const html = readFileSync(new URL("./settings.html", import.meta.url), "utf8");
const messages = JSON.parse(
  readFileSync(new URL("./translations/en.json", import.meta.url), "utf8")
).desktop.settings;
const logging = readFileSync(new URL("./logging.js", import.meta.url), "utf8");
const start = html.indexOf("      async function loadSettingsLoadError(");
const end = html.indexOf("      // Load initial settings", start);
assert.ok(start !== -1 && end > start);

function harness(invoke) {
  const elements = new Map();
  const errors = [];
  const announcements = [];
  const context = vm.createContext({
    invoke,
    Error,
    settingsLoadGeneration: 1,
    settingsLoadStatusRequest: 0,
    announceSettingChange: (message) => announcements.push(message),
    console: { error: (...args) => errors.push(args) },
    t: (key) => messages[key.split(".").at(-1)],
    document: {
      getElementById(id) {
        if (!elements.has(id)) {
          elements.set(id, {
            hidden: true,
            textContent: "",
            dataset: {},
            set innerHTML(_) {
              assert.fail("Diagnostics must never be parsed as HTML");
            },
          });
        }
        return elements.get(id);
      },
    },
  });
  vm.runInContext(logging, context);
  vm.runInContext(html.slice(start, end), context);
  const reportStart = html.indexOf("      function reportSettingSaveFailure(");
  const reportEnd = html.indexOf("      async function saveBooleanSetting(", reportStart);
  assert.ok(reportStart !== -1 && reportEnd > reportStart);
  vm.runInContext(html.slice(reportStart, reportEnd), context);
  return {
    context,
    elements,
    errors,
    announcements,
    load: (generation = 1) => context.loadSettingsLoadError(generation),
  };
}

test("warning is a persistent accessible region at the top of main and checked on load", () => {
  assert.match(
    html,
    /<main>\s*<div\s+id="settings-load-warning"[^>]*role="alert"[^>]*aria-atomic="true"[^>]*hidden\s*>/
  );
  const load = html.slice(html.indexOf("      async function loadSettings()"));
  assert.ok(
    load.indexOf("void loadSettingsLoadError(loadGeneration)") <
      load.indexOf('await invoke("get_settings")')
  );
  assert.ok(html.includes(".then(() => loadSettings())"));
  assert.ok(!html.slice(start, end).includes("setTimeout"));
});

test("a load error appears without saving, as literal text, with recovery guidance", async () => {
  const diagnostic = '<img src=x onerror="alert(1)">: settings.json';
  const h = harness(async (...args) => {
    assert.deepEqual(args, ["get_settings_load_error"]);
    return diagnostic;
  });
  await h.load();
  assert.equal(h.elements.get("settings-load-warning").hidden, false);
  assert.equal(h.elements.get("settings-load-error").textContent, diagnostic);
  assert.equal(h.elements.get("settings-load-error").hidden, false);
  assert.equal(h.elements.get("settings-load-recovery").hidden, false);
  assert.equal(h.elements.get("settings-load-summary").textContent, messages.load_failed);
  assert.match(messages.load_failed_recovery, /Quit.*repair or remove.*restart/);
});

test("healthy settings hide and clear the warning on reload", async () => {
  let error = "settings.json could not load";
  const h = harness(async () => error);
  await h.load();
  error = null;
  await h.load();
  assert.equal(h.elements.get("settings-load-warning").hidden, true);
  assert.equal(h.elements.get("settings-load-error").textContent, "");
});

test("query failure is visible and logs only sanitized diagnostics", async () => {
  const h = harness(async () => {
    throw new Error("https://user:secret@example.com/?token=private");
  });
  await h.load();
  assert.equal(h.elements.get("settings-load-warning").hidden, false);
  assert.equal(
    h.elements.get("settings-load-summary").textContent,
    messages.load_status_unavailable
  );
  assert.equal(h.elements.get("settings-load-recovery").hidden, true);
  assert.equal(h.elements.get("settings-load-error").hidden, true);
  assert.equal(h.errors.length, 1);
  const logged = JSON.stringify(h.errors);
  assert.ok(logged.includes("example.com"));
  assert.ok(!logged.includes("secret"));
  assert.ok(!logged.includes("private"));
});

test("save failure refreshes the warning without invalidating settings handlers", async () => {
  let status = null;
  let calls = 0;
  const h = harness(async (command) => {
    assert.equal(command, "get_settings_load_error");
    calls++;
    return status;
  });
  await h.load();
  status = "settings.json could not be loaded";
  const error = new Error(
    "Save failed: https://user:url-secret@example.com/?token=query-secret " +
      "token=first-secret password=second-secret"
  );
  error.code = "ECONNREFUSED";
  h.context.reportSettingSaveFailure(error);
  await new Promise(setImmediate);
  assert.equal(calls, 2);
  assert.equal(h.context.settingsLoadGeneration, 1);
  assert.equal(h.elements.get("settings-load-warning").hidden, false);
  assert.equal(h.elements.get("settings-load-error").textContent, status);
  assert.deepEqual(h.announcements, [messages.setting_save_failed]);
  assert.equal(h.errors.length, 1);
  assert.equal(h.errors[0][0], "[Settings] Failed to save setting:");
  assert.notEqual(h.errors[0][1], error);
  assert.equal(h.errors[0][1].code, "ECONNREFUSED");
  const logged = JSON.stringify(h.errors);
  assert.ok(logged.includes("Save failed"));
  assert.ok(logged.includes("example.com"));
  assert.ok(!logged.includes("-secret"));
  assert.ok(!/defaults/i.test(messages.load_failed));
  assert.match(messages.load_failed, /cannot be saved.*restarted/);
});

test("non-load save failures retain the generic visible error", async () => {
  const h = harness(async () => null);
  h.context.reportSettingSaveFailure(new Error("Permission denied"));
  await new Promise(setImmediate);
  assert.deepEqual(h.announcements, [messages.setting_save_failed]);
  assert.equal(h.elements.get("settings-load-warning").hidden, true);
});

test("an initial status response cannot hide a newer save-failure warning", async () => {
  let finishInitial;
  let calls = 0;
  const h = harness(() =>
    ++calls === 1
      ? new Promise((resolve) => {
          finishInitial = resolve;
        })
      : Promise.resolve("settings became invalid")
  );
  const initialLoad = h.load();
  h.context.reportSettingSaveFailure(new Error("Save failed"));
  await new Promise(setImmediate);
  assert.equal(h.elements.get("settings-load-warning").hidden, false);
  finishInitial(null);
  await initialLoad;
  assert.equal(h.context.settingsLoadGeneration, 1);
  assert.equal(h.elements.get("settings-load-warning").hidden, false);
  assert.equal(h.elements.get("settings-load-error").textContent, "settings became invalid");
});

test("an older failure cannot replace a newer healthy status in the same load", async () => {
  let finishOld;
  let calls = 0;
  const h = harness(() =>
    ++calls === 1
      ? new Promise((resolve) => {
          finishOld = resolve;
        })
      : Promise.resolve(null)
  );
  const oldLoad = h.load();
  await h.load();
  finishOld("stale failure");
  await oldLoad;
  assert.equal(h.elements.get("settings-load-warning").hidden, true);
  assert.equal(h.elements.get("settings-load-error").textContent, "");
});

test("older status responses cannot overwrite a newer reload", async () => {
  let finishOld;
  let count = 0;
  const h = harness(() =>
    ++count === 1
      ? new Promise((resolve) => {
          finishOld = resolve;
        })
      : Promise.resolve(null)
  );
  const oldLoad = h.load();
  h.context.settingsLoadGeneration = 2;
  await h.load(2);
  finishOld("stale failure");
  await oldLoad;
  assert.equal(h.elements.get("settings-load-warning").hidden, true);
});
