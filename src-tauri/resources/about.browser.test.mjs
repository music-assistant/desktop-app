import { strict as assert } from "node:assert";
import { test } from "node:test";
import { readFileSync, accessSync, statSync, mkdtempSync, rmSync, constants } from "node:fs";
import { tmpdir } from "node:os";
import { join, win32 } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { EventEmitter } from "node:events";

function isExecutable(file) {
  try {
    if (!statSync(file).isFile()) return false;
    accessSync(file, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function findOnPath(name) {
  const lookup = spawnSync(process.platform === "win32" ? "where.exe" : "which", [name], {
    encoding: "utf8",
  });
  return lookup.status === 0 ? lookup.stdout.split(/\r?\n/)[0] : null;
}

function findChrome({ env, platform, executable, lookup }) {
  // CHROME_BIN is an executable path, not a shell command or browser arguments.
  if (env.CHROME_BIN) {
    if (!executable(env.CHROME_BIN))
      throw new Error(`CHROME_BIN is not executable: ${env.CHROME_BIN}`);
    return env.CHROME_BIN;
  }
  const candidates =
    platform === "darwin"
      ? ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"]
      : platform === "win32"
        ? [
            env.PROGRAMFILES &&
              win32.join(env.PROGRAMFILES, "Google", "Chrome", "Application", "chrome.exe"),
            env["PROGRAMFILES(X86)"] &&
              win32.join(env["PROGRAMFILES(X86)"], "Google", "Chrome", "Application", "chrome.exe"),
            env.LOCALAPPDATA &&
              win32.join(env.LOCALAPPDATA, "Google", "Chrome", "Application", "chrome.exe"),
          ]
        : [];
  for (const candidate of candidates) {
    if (candidate && executable(candidate)) return candidate;
  }
  for (const name of ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"]) {
    const candidate = lookup(name);
    if (candidate && executable(candidate)) return candidate;
  }
  return null;
}

const chrome = findChrome({
  env: process.env,
  platform: process.platform,
  executable: isExecutable,
  lookup: findOnPath,
});
const english = JSON.parse(readFileSync(new URL("./translations/en.json", import.meta.url)));
const dutch = JSON.parse(readFileSync(new URL("./translations/nl.json", import.meta.url)));
const german = JSON.parse(readFileSync(new URL("./translations/de.json", import.meta.url)));

function waitForBrowserPort(browser, executable, timeoutMs = 20000) {
  return new Promise((resolve, reject) => {
    let stderr = "";
    const diagnostics = () =>
      `Chrome (${executable}, pid ${browser.pid ?? "unknown"}): ${stderr.slice(-2000) || "no stderr"}`;
    const cleanup = () => {
      clearTimeout(timer);
      browser.stderr.off("data", onData);
      browser.off("error", onError);
      browser.off("exit", onExit);
    };
    const fail = (message) => {
      cleanup();
      reject(new Error(`${message}; ${diagnostics()}`));
    };
    const onData = (chunk) => {
      stderr = (stderr + chunk.toString()).slice(-4096);
      const match = stderr.match(
        /DevTools listening on (ws:\/\/127\.0\.0\.1:\d+\/devtools\/browser\/[^\s]+)\r?\n/
      );
      if (!match) return;
      const port = new URL(match[1]).port;
      cleanup();
      resolve(port);
    };
    const onError = (error) => fail(`Chrome failed to start: ${error.message}`);
    const onExit = (code, signal) =>
      fail(`Chrome exited before debugging became available (code ${code}, signal ${signal})`);
    const timer = setTimeout(
      () => fail(`Chrome debugging endpoint did not become available within ${timeoutMs}ms`),
      timeoutMs
    );
    browser.stderr.on("data", onData);
    browser.on("error", onError);
    browser.on("exit", onExit);
  });
}

async function browserTest() {
  const profile = mkdtempSync(join(tmpdir(), "about-browser-"));
  const browser = spawn(
    chrome,
    [
      "--headless",
      "--no-sandbox",
      "--disable-dev-shm-usage",
      "--no-first-run",
      "--no-default-browser-check",
      "--disable-gpu",
      "--remote-debugging-port=0",
      `--user-data-dir=${profile}`,
      "about:blank",
    ],
    { stdio: ["ignore", "ignore", "pipe"] }
  );
  try {
    const port = await waitForBrowserPort(browser, chrome);
    const targets = await (await fetch(`http://127.0.0.1:${port}/json`)).json();
    const page = targets.find((target) => target.type === "page");
    assert.ok(page, "Chrome must provide a page target");
    const socket = new WebSocket(page.webSocketDebuggerUrl);
    await new Promise((resolve, reject) => {
      socket.addEventListener("open", resolve, { once: true });
      socket.addEventListener("error", reject, { once: true });
    });
    let id = 0;
    const pending = new Map();
    socket.addEventListener("message", ({ data }) => {
      const message = JSON.parse(data);
      if (pending.has(message.id)) {
        const { resolve, reject } = pending.get(message.id);
        pending.delete(message.id);
        message.error ? reject(new Error(message.error.message)) : resolve(message.result);
      }
    });
    const send = (method, params = {}) =>
      new Promise((resolve, reject) => {
        const next = ++id;
        pending.set(next, { resolve, reject });
        socket.send(JSON.stringify({ id: next, method, params }));
      });
    const evaluate = async (expression) => {
      const response = await send("Runtime.evaluate", {
        expression,
        awaitPromise: true,
        returnByValue: true,
      });
      if (response.exceptionDetails)
        throw new Error(
          response.exceptionDetails.exception?.description || response.exceptionDetails.text
        );
      return response.result.value;
    };
    try {
      await send("Page.enable");
      await send("Runtime.enable");
      await send("Runtime.addBinding", { name: "__aboutResize" });
      let resizeError;
      let resizeCount = 0;
      socket.addEventListener("message", ({ data }) => {
        const message = JSON.parse(data);
        if (message.method !== "Runtime.bindingCalled" || message.params.name !== "__aboutResize")
          return;
        const { id, width, height } = JSON.parse(message.params.payload);
        void (async () => {
          await send("Emulation.setDeviceMetricsOverride", {
            width,
            height,
            deviceScaleFactor: 1,
            mobile: false,
          });
          for (let attempt = 0; attempt < 100; attempt++) {
            const viewport = await evaluate("({width: innerWidth, height: innerHeight})");
            if (viewport.width === width && viewport.height === height) {
              await evaluate(`window.__aboutMock.ackResize(${id})`);
              resizeCount++;
              return;
            }
            await new Promise((resolve) => setTimeout(resolve, 10));
          }
          throw new Error(`Chrome viewport did not reach ${width}x${height}`);
        })().catch((error) => {
          resizeError = error;
        });
      });
      await send("Emulation.setDeviceMetricsOverride", {
        width: 460,
        height: 350,
        deviceScaleFactor: 1,
        mobile: false,
      });
      const mock = `(() => {
        const messages = ${JSON.stringify({ en: english, de: german, nl: dutch })};
        const titlebar = 28;
        let size = {width: 460, height: 350 + titlebar};
        let available = {width: 900, height: 900};
        let locale = 'en';
        let version = '0.7.2';
        let resizeId = 0;
        const pendingResizes = new Map();
        const win = {
          innerSize: async () => ({...size}),
          outerSize: async () => ({...size}),
          outerPosition: async () => ({x: 0, y: 0}),
          setPosition: async () => {},
          setSize: ({width, height}) => new Promise((resolve) => {
            const id = ++resizeId;
            pendingResizes.set(id, () => {
              size = {width, height};
              resolve();
            });
            window.__aboutResize(JSON.stringify({id, width, height: height - titlebar}));
          }),
        };
        window.__aboutMock = {size: () => ({...size}), titlebar, ackResize: (id) => pendingResizes.get(id)(),
          setAvailable: (width, height) => { available = {width, height}; },
          setLocale: (value) => { locale = value; }, setVersion: (value) => { version = value; }};
        window.__TAURI__ = {
          core: {invoke: async (command) => command === 'get_i18n_bundle'
            ? {locale, messages: messages[locale], fallback_messages: messages.en} : version},
          window: {getCurrentWindow: () => win,
            currentMonitor: async () => ({workArea: {position: {x: 0, y: 0}, size: available}, scaleFactor: 1}),
            LogicalSize: class {constructor(width, height) {this.width = width; this.height = height;}},
            PhysicalPosition: class {constructor(x, y) {this.x = x; this.y = y;}}},
        };
      })();`;
      await send("Page.addScriptToEvaluateOnNewDocument", { source: mock });
      await send("Page.navigate", { url: new URL("./about.html", import.meta.url).href });
      for (let i = 0; i < 100; i++) {
        if (
          await evaluate(
            "document.readyState === 'complete' && !!document.querySelector('main') && !!window.__aboutMock"
          )
        )
          break;
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
      const waitForFit = async () => {
        for (let attempt = 0; attempt < 100; attempt++) {
          if (resizeError) throw resizeError;
          const state = await evaluate(
            "({width: innerWidth, height: innerHeight, size: window.__aboutMock.size(), titlebar: window.__aboutMock.titlebar})"
          );
          if (
            resizeCount > 0 &&
            state.width === state.size.width &&
            state.height + state.titlebar === state.size.height
          )
            return state;
          await new Promise((resolve) => setTimeout(resolve, 20));
        }
        throw new Error("About did not settle to its native and rendered dimensions");
      };
      await waitForFit();
      const normal = await evaluate(
        "({width: innerWidth, height: innerHeight, scrollWidth: document.documentElement.scrollWidth, scrollHeight: document.documentElement.scrollHeight, titleHeight: document.querySelector('h1').getBoundingClientRect().height, size: window.__aboutMock.size(), titlebar: window.__aboutMock.titlebar})"
      );
      assert.equal(normal.width, normal.size.width);
      assert.equal(normal.height + normal.titlebar, normal.size.height);
      assert.equal(normal.titlebar, 28);
      assert.ok(resizeCount > 0, "the About script must request a native resize");
      assert.ok(normal.scrollWidth <= normal.width, JSON.stringify(normal));
      assert.ok(normal.scrollHeight <= normal.height, JSON.stringify(normal));
      const initialHeight = normal.size.height;
      await evaluate("document.querySelector('h1').style.maxWidth = '270px'");
      for (let attempt = 0; attempt < 100; attempt++) {
        if (resizeError) throw resizeError;
        const state = await evaluate(
          "({height: innerHeight, titleHeight: document.querySelector('h1').getBoundingClientRect().height, size: window.__aboutMock.size()})"
        );
        if (
          state.titleHeight > normal.titleHeight &&
          state.size.height > initialHeight &&
          state.size.height === state.height + normal.titlebar
        )
          break;
        await new Promise((resolve) => setTimeout(resolve, 20));
      }
      const wrapped = await evaluate(
        "({height: innerHeight, scrollHeight: document.documentElement.scrollHeight, titleHeight: document.querySelector('h1').getBoundingClientRect().height, size: window.__aboutMock.size()})"
      );
      assert.ok(wrapped.titleHeight > normal.titleHeight, JSON.stringify(wrapped));
      assert.ok(wrapped.size.height > initialHeight, JSON.stringify(wrapped));
      assert.equal(wrapped.height + normal.titlebar, wrapped.size.height);
      assert.ok(wrapped.scrollHeight <= wrapped.height, JSON.stringify(wrapped));
      for (const locale of ["de", "nl"]) {
        await evaluate(
          `window.initI18n({locale: ${JSON.stringify(locale)}, messages: ${JSON.stringify({ de: german, nl: dutch })}[${JSON.stringify(locale)}], fallback_messages: ${JSON.stringify(english)}}); window.applyTranslations(); document.getElementById('version').textContent = window.t('desktop.about.version', '0.7.2');`
        );
        const localized = await evaluate(
          "({heading: document.querySelector('h1').textContent, notes: document.getElementById('release-notes').textContent})"
        );
        assert.equal(localized.heading, { de: german, nl: dutch }[locale].desktop.about.app_name);
        assert.equal(
          localized.notes,
          { de: german, nl: dutch }[locale].desktop.about.release_notes
        );
      }
      await evaluate(
        "document.querySelector('h1').textContent = 'A very long accessibility heading '.repeat(16); document.getElementById('release-notes').textContent = 'LongUnbreakableReleaseNotes'.repeat(20); document.body.style.fontSize = '32px'; window.__aboutMock.setAvailable(320, 260)"
      );
      for (let attempt = 0; attempt < 100; attempt++) {
        if (resizeError) throw resizeError;
        const state = await evaluate(
          "({width: innerWidth, height: innerHeight, size: window.__aboutMock.size()})"
        );
        if (
          state.width === 320 &&
          state.height === 232 &&
          state.size.width === state.width &&
          state.size.height === state.height + normal.titlebar
        )
          break;
        await new Promise((resolve) => setTimeout(resolve, 20));
      }
      if (resizeError) throw resizeError;
      const metrics = await evaluate(
        "({viewport: innerWidth, height: innerHeight, native: window.__aboutMock.size(), scrollWidth: document.documentElement.scrollWidth, scrollHeight: document.documentElement.scrollHeight, mainWidth: document.querySelector('main').getBoundingClientRect().width, mainHeight: document.querySelector('main').getBoundingClientRect().height})"
      );
      assert.equal(metrics.viewport, 320);
      assert.equal(metrics.height, 232);
      assert.equal(metrics.native.width, metrics.viewport);
      assert.equal(metrics.native.height, metrics.height + normal.titlebar);
      assert.ok(metrics.scrollWidth <= metrics.viewport, JSON.stringify(metrics));
      assert.ok(metrics.mainWidth <= metrics.viewport - 48, JSON.stringify(metrics));
      assert.ok(metrics.scrollHeight > metrics.height, JSON.stringify(metrics));
    } finally {
      socket.close();
    }
  } finally {
    browser.kill();
    if (browser.pid && browser.exitCode === null && browser.signalCode === null) {
      await new Promise((resolve) => browser.once("exit", resolve));
    }
    rmSync(profile, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  }
}

test("Chrome discovery honors executable overrides and platform browser locations", () => {
  const visited = [];
  const executable = (path) =>
    path === "/custom/chrome" ||
    path === "/usr/bin/chromium" ||
    path === "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe";
  const lookup = (name) => {
    visited.push(name);
    return name === "chromium" ? "/usr/bin/chromium" : null;
  };
  assert.equal(
    findChrome({ env: { CHROME_BIN: "/custom/chrome" }, platform: "linux", executable, lookup }),
    "/custom/chrome"
  );
  assert.deepEqual(visited, [], "an override must not run a PATH lookup");
  assert.throws(
    () => findChrome({ env: { CHROME_BIN: "echo unsafe" }, platform: "linux", executable, lookup }),
    /CHROME_BIN is not executable/
  );
  assert.deepEqual(visited, [], "invalid overrides must not be interpreted as shell commands");
  assert.equal(findChrome({ env: {}, platform: "linux", executable, lookup }), "/usr/bin/chromium");
  assert.deepEqual(visited, ["google-chrome", "google-chrome-stable", "chromium"]);
  assert.equal(
    findChrome({
      env: { PROGRAMFILES: "C:\\Program Files" },
      platform: "win32",
      executable,
      lookup,
    }),
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe"
  );
  assert.equal(
    findChrome({
      env: {},
      platform: "darwin",
      executable: (path) => path.includes("Google Chrome.app"),
      lookup,
    }),
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
  );
  assert.equal(
    findChrome({ env: {}, platform: "linux", executable: () => false, lookup: () => null }),
    null
  );
});

test("Chrome startup reads a split debugging endpoint from stderr", async () => {
  const browser = new EventEmitter();
  browser.stderr = new EventEmitter();
  browser.pid = 123;
  const ready = waitForBrowserPort(browser, "/test/chrome", 100);
  browser.stderr.emit(
    "data",
    Buffer.from("[startup] DevTools listening on ws://127.0.0.1:42/devtools/brow")
  );
  browser.stderr.emit("data", Buffer.from("ser/example\n"));
  assert.equal(await ready, "42");
  assert.equal(browser.listenerCount("exit"), 0);
  assert.equal(browser.stderr.listenerCount("data"), 0);
});

test("Chrome startup reports early exit, spawn failure, and bounded timeout", async () => {
  const fakeBrowser = () => {
    const browser = new EventEmitter();
    browser.stderr = new EventEmitter();
    return browser;
  };
  const exited = fakeBrowser();
  const exitReady = waitForBrowserPort(exited, "/test/chrome", 100);
  exited.stderr.emit("data", Buffer.from("profile unavailable\n"));
  exited.emit("exit", 1, null);
  await assert.rejects(exitReady, /Chrome \(\/test\/chrome, pid unknown\): profile unavailable/);
  const failed = fakeBrowser();
  const spawnReady = waitForBrowserPort(failed, "/test/chrome", 100);
  failed.emit("error", new Error("ENOENT"));
  await assert.rejects(spawnReady, /Chrome failed to start: ENOENT/);
  const timedOut = fakeBrowser();
  const timeoutReady = waitForBrowserPort(timedOut, "/test/chrome", 10);
  timedOut.stderr.emit("data", Buffer.from("startup stalled"));
  await assert.rejects(
    timeoutReady,
    /within 10ms; Chrome \(\/test\/chrome, pid unknown\): startup stalled/
  );
});

test(
  "About stays horizontally accessible and vertically scrollable with large translated content",
  {
    skip:
      !chrome &&
      !process.env.CI &&
      "Chrome not found; set CHROME_BIN to an executable browser path",
    timeout: 30000,
  },
  async () => {
    assert.ok(
      chrome,
      "Chrome not found in CI; install Chrome or set CHROME_BIN to an executable browser path"
    );
    await browserTest();
  }
);
