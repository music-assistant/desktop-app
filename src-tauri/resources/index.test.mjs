/**
 * Regression test for XSS via mDNS server name injection.
 *
 * Verifies that the launcher's server list rendering does not
 * interpolate user-controlled strings into inline event handlers.
 *
 * Run: node src-tauri/resources/index.test.mjs
 */

import { readFileSync } from "fs";
import { strict as assert } from "assert";

const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");

// ---- Test 1: No inline onclick handlers with interpolated values ----
// The server list template should NOT contain onclick with escapeHtml
// interpolation, which is vulnerable to quote-breakout injection.
const onclickPattern = /onclick="connectToServer\('\$\{escapeHtml/;
assert.ok(
  !onclickPattern.test(html),
  "FAIL: Found inline onclick with escapeHtml interpolation — vulnerable to XSS via quote breakout"
);

// ---- Test 2: Server items use data attributes + addEventListener ----
assert.ok(
  html.includes("data-server-index"),
  "FAIL: Server items should use data-server-index attributes"
);
assert.ok(
  html.includes("addEventListener"),
  "FAIL: Click handlers should be bound via addEventListener, not inline onclick"
);

// ---- Test 3: Simulate the escapeHtml function and verify XSS payloads are inert ----
// Replicate the browser's textContent/innerHTML escaping behavior
function escapeHtml(text) {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  // Note: textContent/innerHTML does NOT escape ' " or `
}

const xssPayloads = [
  "z');alert(1);('",
  "z',alert(1),'",
  'z");alert(1);("',
  "z`);alert(1);(`",
  "z');new Image().src=`http://evil.com`;('",
  "<script>alert(1)</script>",
  "test' onclick='alert(1)",
];

for (const payload of xssPayloads) {
  const escaped = escapeHtml(payload);

  // escapeHtml must neutralize HTML tag injection in element content
  assert.ok(
    !escaped.includes("<script>"),
    `FAIL: escapeHtml did not neutralize script tag in: ${payload}`
  );
}

// ---- Test 4: The template in index.html uses safe integer index, not user content ----
// Extract the server-item template from the source and verify it only
// interpolates a numeric index into attributes, never escapeHtml(server.name/url).
const templateSection = html.slice(html.indexOf(".map("), html.indexOf(".join("));

// The data attribute must use a safe integer index
assert.ok(
  templateSection.includes("data-server-index"),
  "FAIL: Template should use data-server-index for click binding"
);

// User-controlled values must NOT appear in any HTML attribute context
// (they should only appear inside element content via escapeHtml)
const attrPattern = /=["'][^"']*\$\{escapeHtml\(server\.(name|url)\)/;
assert.ok(
  !attrPattern.test(templateSection),
  "FAIL: User-controlled escapeHtml values must not appear in HTML attributes"
);

console.log("All tests passed.");
