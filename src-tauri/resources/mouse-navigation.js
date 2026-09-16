// Mouse side-button navigation for Tauri's embedded WebView.
//
// This initialization script runs on every page load, including the remote Music
// Assistant frontend. It maps the mouse "back" and "forward" side buttons to
// history navigation, so users can navigate the way they do in a browser.
(function () {
  if (!window.__TAURI_INTERNALS__) return;

  // MouseEvent.button 3 is the "back" side button, 4 is "forward". Handle them
  // in the capture phase so page handlers cannot swallow the event first.
  window.addEventListener(
    "mouseup",
    function (event) {
      if (event.button === 3) {
        event.preventDefault();
        window.history.back();
      } else if (event.button === 4) {
        event.preventDefault();
        window.history.forward();
      }
    },
    true
  );
})();
