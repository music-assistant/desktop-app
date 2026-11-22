import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const source = readFileSync(new URL("../src/initial_navigation.rs", import.meta.url), "utf8");
const match = source.match(/fn publication_script\([\s\S]*?\n\s*r"([\s\S]*?)"\n\s*\)\n\}/);
assert.ok(match, "native publication template must be available to test");
const template = match[1]
  .replaceAll("{origin}", '"https://example.org"')
  .replaceAll("{path}", '"/music/"')
  .replaceAll("{secret}", '"nonce-one"')
  .replaceAll("{{", "{")
  .replaceAll("}}", "}");

function run({ origin, path, top = true, script = template }) {
  const events = [];
  const window = { dispatchEvent: (event) => events.push(event.type) };
  window.top = top ? window : {};
  const context = vm.createContext({
    window,
    location: { origin, pathname: path },
    Event: class {
      constructor(type) {
        this.type = type;
      }
    },
  });
  vm.runInContext(script, context);
  return { window, context, events };
}

const approved = run({ origin: "https://example.org", path: "/music/login" });
assert.equal(approved.window.__COMPANION_SAVED_LOGIN_NONCE__, "nonce-one");
assert.deepEqual(approved.events, ["companion-saved-login-ready"]);
vm.runInContext(template, approved.context);
assert.equal(approved.window.__COMPANION_SAVED_LOGIN_NONCE__, "nonce-one");
assert.deepEqual(approved.events, ["companion-saved-login-ready"]);
for (const denied of [
  { origin: "https://example.org", path: "/music/login", top: false },
  { origin: "https://other.example", path: "/music/login" },
  { origin: "https://example.org", path: "/music-other/login" },
]) {
  const result = run(denied);
  assert.equal(result.window.__COMPANION_SAVED_LOGIN_NONCE__, undefined);
  assert.deepEqual(result.events, []);
}
const rotated = run({
  origin: "https://example.org",
  path: "/music/login",
  script: template.replaceAll("nonce-one", "nonce-two"),
});
assert.equal(rotated.window.__COMPANION_SAVED_LOGIN_NONCE__, "nonce-two");
console.log(
  "native nonce publication script: approved top document, duplicate, rotation, child and off-scope denial passed"
);
