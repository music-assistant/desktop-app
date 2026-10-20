import { readFileSync } from "fs";
import { strict as assert } from "assert";
import vm from "node:vm";

const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
const source = html.slice(
  html.indexOf("      // Try to auto-connect to last server."),
  html.indexOf("      // Manual connection")
);
assert.ok(source.includes('invoke("reserve_initial_navigation")'));

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
function tick() {
  return new Promise((resolve) => setImmediate(resolve));
}

function setup() {
  const calls = [];
  const elements = new Map();
  const element = (id) => {
    if (!elements.has(id))
      elements.set(id, {
        classList: {
          add() {},
          remove() {},
          contains() {
            return false;
          },
        },
        style: {},
        focus() {},
        select() {},
        scrollIntoView() {},
        dataset: {},
        textContent: "",
      });
    return elements.get(id);
  };
  const context = {
    connectAttemptId: 0,
    activeNavigationToken: null,
    autoConnectAttempted: false,
    invoke(cmd, args) {
      const reply = deferred();
      calls.push({ cmd, args, ...reply });
      if (cmd === "cancel_initial_navigation") reply.resolve();
      return reply.promise;
    },
    document: { getElementById: element, addEventListener() {} },
    window: {
      stop() {
        calls.push({ cmd: "stop" });
      },
      location: {
        replace(url) {
          calls.push({ cmd: "replace", url });
        },
      },
    },
    t(key) {
      return key;
    },
    console: { warn() {} },
    loadLastServer() {},
  };
  vm.createContext(context);
  vm.runInContext(source, context);
  return { context, calls };
}
function callsOf(calls, cmd) {
  return calls.filter((call) => call.cmd === cmd);
}

{
  const { context, calls } = setup();
  const a = context.connectToServer("http://a");
  context.cancelAutoConnect();
  callsOf(calls, "reserve_initial_navigation")[0].resolve(1);
  await a;
  assert.equal(callsOf(calls, "begin_initial_navigation").length, 0);
  assert.deepEqual(
    callsOf(calls, "cancel_initial_navigation").map((call) => call.args.generation),
    [1]
  );
}
{
  const { context, calls } = setup();
  const a = context.connectToServer("http://a");
  callsOf(calls, "reserve_initial_navigation")[0].resolve(1);
  await tick();
  context.cancelAutoConnect();
  assert.deepEqual(
    callsOf(calls, "cancel_initial_navigation").map((call) => call.args.generation),
    [1]
  );
  callsOf(calls, "begin_initial_navigation")[0].resolve("http://a/");
  await a;
  assert.equal(callsOf(calls, "replace").length, 0);
}
{
  const { context, calls } = setup();
  const a = context.connectToServer("http://a");
  callsOf(calls, "reserve_initial_navigation")[0].resolve(1);
  await tick();
  const b = context.connectToServer("http://b");
  callsOf(calls, "reserve_initial_navigation")[1].resolve(2);
  await tick();
  callsOf(calls, "begin_initial_navigation")[0].resolve("http://a/");
  await a;
  assert.ok(
    !callsOf(calls, "cancel_initial_navigation").some((call) => call.args.generation === 2)
  );
  callsOf(calls, "begin_initial_navigation")[1].resolve("http://b/");
  await b;
  assert.deepEqual(
    callsOf(calls, "replace").map((call) => call.url),
    ["http://b/"]
  );
}
for (const mode of ["auto", "manual"]) {
  const { context, calls } = setup();
  const el = context.document.getElementById;
  context.loadLastServer = async () => ({ url: "http://a", name: "A" });
  const a = mode === "auto" ? context.tryAutoConnect() : context.connectToServer("http://a", "A");
  await tick();
  callsOf(calls, "reserve_initial_navigation")[0].resolve(1);
  await tick();
  callsOf(calls, "begin_initial_navigation")[0].reject("connection");
  assert.equal(await a, false, mode);
  const message = "desktop.launcher.connection_failed_message_generic";
  assert.equal(el("errorMsg").textContent, message, mode);
  assert.equal(el("errorMsg").style.display, "block", mode);
  assert.equal(el("serverStatus").textContent, message, mode);
  assert.equal(el("recoveryActions").style.display, "block", mode);
  el("editAddress").onclick();
  assert.equal(el("manualAddress").value, "http://a", mode);
  el("retryConnection").onclick();
  assert.equal(callsOf(calls, "reserve_initial_navigation").length, 2, mode);
}
assert.match(
  readFileSync(new URL("../src/initial_navigation.rs", import.meta.url), "utf8"),
  /window === window\.top/
);
const lib = readFileSync(new URL("../src/lib.rs", import.meta.url), "utf8");
const replace = lib.slice(
  lib.indexOf("fn replace_with_launcher("),
  lib.indexOf("fn navigate_to_launcher(")
);
assert.ok(
  replace.indexOf(".visible(false)") < replace.indexOf(".build()"),
  "Replacement launcher starts hidden"
);
const approvalIndex = replace.indexOf("may_replace(old)");
const showIndex = replace.indexOf("new_window.show()");
assert.ok(approvalIndex !== -1 && showIndex !== -1, "Swap approval and show calls exist");
assert.ok(approvalIndex < showIndex, "Shown only after the swap is accepted");
console.log("Initial navigation ordering tests passed.");
