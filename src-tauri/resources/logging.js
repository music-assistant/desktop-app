// Sanitize diagnostics only. Never use this output for navigation or persistence.
(function (root) {
  function redactText(value) {
    return value
      .replace(/[a-z][a-z\d+.-]*:\/\/[^\s"'<>`]+/gi, (url) => {
        const boundary = url.indexOf("://") + 3;
        const end = url.slice(boundary).search(/[/?#]/);
        const authorityEnd = end < 0 ? url.length : boundary + end;
        const authority = url.slice(boundary, authorityEnd);
        const at = authority.lastIndexOf("@");
        const safeAuthority = at < 0 ? authority : "[REDACTED]@" + authority.slice(at + 1);
        const path = url.slice(authorityEnd).replace(/[?#].*$/, "[REDACTED]");
        return url.slice(0, boundary) + safeAuthority + path;
      })
      .replace(
        /\b(authorization)(["']?\s*[:=]\s*)("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\r\n&,;}]+)/gi,
        (match, key, separator) => key + separator + "[REDACTED]"
      )
      .replace(
        /\b(token|access_token|refresh_token|id_token|password|client_secret|api_key)(["']?\s*[:=]\s*)("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s&,;}]+)/gi,
        (match, key, separator) => key + separator + "[REDACTED]"
      );
  }

  function redactDiagnostic(value, seen = new WeakSet()) {
    if (typeof value === "string") return redactText(value);
    if (value === null || typeof value !== "object") return value;
    if (seen.has(value)) return "[Circular]";
    seen.add(value);
    const result = Array.isArray(value) ? [] : {};
    // Error properties are non-enumerable; keep the message, stack, and cause.
    const keys = new Set(Object.keys(value));
    if (value instanceof Error) {
      for (const key of ["name", "message", "stack", "cause"]) {
        if (key in value) keys.add(key);
      }
    }
    for (const key of keys) {
      const safeKey = redactText(key);
      let safeValue;
      try {
        safeValue =
          /^(token|access_token|refresh_token|id_token|authorization|password|client_secret|api_key)$/i.test(
            key
          )
            ? "[REDACTED]"
            : redactDiagnostic(value[key], seen);
      } catch {
        safeValue = "[Unreadable]";
      }
      Object.defineProperty(result, safeKey, {
        value: safeValue,
        enumerable: true,
        configurable: true,
      });
    }
    return result;
  }

  root.redactDiagnostic = redactDiagnostic;
})(globalThis);
