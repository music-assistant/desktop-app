// Polyfill navigator.clipboard for Tauri's embedded WKWebView.
// The browser Clipboard API may fail in embedded webviews due to
// secure context requirements (HTTP vs HTTPS) or missing entitlements.
// This bridges clipboard operations to the native Tauri clipboard plugin.
(function () {
  if (!window.__TAURI_INTERNALS__) return;

  const invoke = window.__TAURI_INTERNALS__.invoke;

  const tauriClipboard = {
    writeText(text) {
      return invoke("plugin:clipboard-manager|write_text", {
        text: String(text),
      });
    },
    readText() {
      return invoke("plugin:clipboard-manager|read_text");
    },
    // Preserve any existing methods we don't override
    write:
      navigator.clipboard && navigator.clipboard.write
        ? navigator.clipboard.write.bind(navigator.clipboard)
        : undefined,
    read:
      navigator.clipboard && navigator.clipboard.read
        ? navigator.clipboard.read.bind(navigator.clipboard)
        : undefined,
    addEventListener:
      navigator.clipboard && navigator.clipboard.addEventListener
        ? navigator.clipboard.addEventListener.bind(navigator.clipboard)
        : function () {},
    removeEventListener:
      navigator.clipboard && navigator.clipboard.removeEventListener
        ? navigator.clipboard.removeEventListener.bind(navigator.clipboard)
        : function () {},
    dispatchEvent:
      navigator.clipboard && navigator.clipboard.dispatchEvent
        ? navigator.clipboard.dispatchEvent.bind(navigator.clipboard)
        : function () {},
  };

  Object.defineProperty(navigator, "clipboard", {
    get: function () {
      return tauriClipboard;
    },
    configurable: true,
  });
})();
