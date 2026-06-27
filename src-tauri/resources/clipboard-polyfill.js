// Companion bridge for Tauri's embedded WebView.
//
// This initialization script runs on every page load, including the remote Music
// Assistant frontend. It exposes the native invoke bridge in the shape the
// frontend expects and provides native fallbacks for WebView APIs that are often
// unavailable on local HTTP origins.
(function () {
  if (!window.__TAURI_INTERNALS__) return;

  const invoke = window.__TAURI_INTERNALS__.invoke;

  // The MA frontend checks for __TAURI__ or __COMPANION__ to enable companion
  // integrations. Some remote origins do not receive the full global Tauri API,
  // while __TAURI_INTERNALS__ is still available to initialization scripts.
  if (!window.__COMPANION__) {
    Object.defineProperty(window, "__COMPANION__", {
      value: { invoke },
      configurable: true,
    });
  }

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
