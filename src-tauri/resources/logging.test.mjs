// Run: node src-tauri/resources/logging.test.mjs
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import assert from "node:assert/strict";

const context = { Error };
runInNewContext(readFileSync(new URL("./logging.js", import.meta.url), "utf8"), context);
const { redactDiagnostic } = context;
const url = "https://alice:credential@host:8095/path?code=url-secret#fragment-secret";
const cause = new Error("Connection refused at " + url);
const error = new Error("Request timed out: " + url, { cause });
error.status = 503;
error.details = { access_token: "field-secret", retry: true };
const safe = redactDiagnostic(error);
const serialized = JSON.stringify(safe);
for (const secret of ["alice", "credential", "url-secret", "fragment-secret", "field-secret"]) {
  assert.ok(!serialized.includes(secret), `Leaked ${secret}`);
}
assert.equal(safe.name, "Error");
assert.ok(safe.message.includes("Request timed out"));
assert.ok(safe.message.includes("host:8095/path"));
assert.ok(safe.stack.includes("logging.test.mjs"));
assert.ok(safe.cause.message.includes("Connection refused"));
assert.equal(safe.status, 503);
assert.equal(safe.details.retry, true);
assert.equal(error.details.access_token, "field-secret", "Do not mutate original diagnostics");
assert.ok(error.message.includes(url), "Do not mutate original errors");

for (const text of [
  'token="quoted-secret" failed',
  "Authorization: Bearer bearer-secret",
  "https://host/?%63ode=encoded-secret&token=duplicate-secret",
  "wss://host/#/callback?token=fragment-secret",
]) {
  assert.ok(!redactDiagnostic(text).includes("-secret"));
}
assert.equal(redactDiagnostic("status 503: connection refused"), "status 503: connection refused");
assert.equal(
  redactDiagnostic("token=hidden connection refused"),
  "token=[REDACTED] connection refused"
);
for (const text of [
  "token=first-secret password=second-secret",
  "password=first-secret token=second-secret api_key=third-secret",
  "token=first-secret Authorization: Bearer second-secret extra-secret",
  "Authorization: Bearer first-secret extra-secret; password=second-secret",
  'Authorization="Bearer first-secret" token=second-secret',
  "Authorization: Bearer first-secret, Authorization: Basic second-secret",
  "Authorization: Bearer first-secret\npassword=second-secret",
]) {
  assert.ok(!redactDiagnostic(text).includes("-secret"), `Leaked adjacent credential: ${text}`);
}
assert.equal(
  redactDiagnostic("Authorization: Custom first second third; status=503"),
  "Authorization: [REDACTED]; status=503"
);
for (const code of ["ECONNREFUSED", -2147467259]) {
  assert.equal(redactDiagnostic({ code }).code, code);
  assert.equal(redactDiagnostic(`code=${code}`), `code=${code}`);
}
const diagnostic = { status: 503 };
Object.defineProperty(diagnostic, "details", {
  enumerable: true,
  get() {
    throw new Error("token=getter-secret");
  },
});
const safeDiagnostic = redactDiagnostic(diagnostic);
assert.equal(safeDiagnostic.details, "[Unreadable]");
assert.equal(safeDiagnostic.status, 503);
assert.ok(!JSON.stringify(safeDiagnostic).includes("getter-secret"));
const circular = { message: "failed" };
circular.self = circular;
assert.equal(redactDiagnostic(circular).self, "[Circular]");

for (const page of ["index.html", "settings.html", "about.html"]) {
  const source = readFileSync(new URL(`./${page}`, import.meta.url), "utf8");
  assert.ok(
    source.includes('<script src="logging.js"></script>'),
    `${page} loads shared sanitizer`
  );
  // Every error-bearing console call must retain details through the sanitizer.
  for (const match of source.matchAll(
    /catch\s*\(\s*(\w+)\s*\)\s*(?:=>\s*)?\{?\s*console\.(?:log|warn|error)\(([^;]+)\);/g
  )) {
    assert.ok(
      match[2].includes(`redactDiagnostic(${match[1]})`),
      `${page}: preserve sanitized error details`
    );
  }
}
const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
assert.ok(html.includes('invoke("server_connect_failed", { url, error: reason })'));
assert.ok(html.includes("window.location.replace(url)"));
assert.ok(html.includes('key: "last_server_url", value: url'));
assert.ok(html.includes("redactDiagnostic(url)"), "Connection diagnostics retain a sanitized URL");
console.log("Logging redaction tests passed.");
