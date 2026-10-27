const RELEASES_URL = "https://github.com/music-assistant/desktop-app/releases";

function releaseNotesUrl(version) {
  if (
    typeof version !== "string" ||
    !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version) ||
    version === "0.0.0"
  ) {
    return RELEASES_URL;
  }
  return `${RELEASES_URL}/tag/${encodeURIComponent(version)}`;
}

const invoke = window.__TAURI__?.core?.invoke;
const windowApi = window.__TAURI__?.window;

async function fitAboutWindow() {
  if (!windowApi) return;
  const aboutWindow = windowApi.getCurrentWindow();
  const main = document.querySelector("main");
  const style = window.getComputedStyle(document.body);
  const horizontalPadding = parseFloat(style.paddingLeft) + parseFloat(style.paddingRight);
  const verticalPadding = parseFloat(style.paddingTop) + parseFloat(style.paddingBottom);
  const [monitor, innerSize, outerSize] = await Promise.all([
    windowApi.currentMonitor(),
    aboutWindow.innerSize(),
    aboutWindow.outerSize(),
  ]);
  const scale = monitor?.scaleFactor || (await aboutWindow.scaleFactor()) || 1;
  // On macOS innerSize can include native titlebar space. Measure the actual
  // WebView viewport instead of assuming that innerSize equals window.innerHeight.
  const frameHeight = outerSize.height / scale - window.innerHeight;
  const frameWidth = outerSize.width / scale - window.innerWidth;
  const availableWidth = window.screen?.availWidth;
  const availableHeight = window.screen?.availHeight;
  const maxWidth = Math.max(
    1,
    Math.floor(
      monitor
        ? monitor.workArea.size.width / scale - frameWidth
        : Number.isFinite(availableWidth) && availableWidth > 0
          ? availableWidth - frameWidth
          : window.innerWidth
    )
  );
  const measure = main.cloneNode(true);
  measure.removeAttribute("id");
  measure.querySelectorAll("[id]").forEach((element) => element.removeAttribute("id"));
  measure.setAttribute("aria-hidden", "true");
  measure.inert = true;
  Object.assign(measure.style, {
    position: "fixed",
    top: "0",
    left: "0",
    visibility: "hidden",
    width: "max-content",
    maxWidth: "none",
  });
  document.body.appendChild(measure);
  let width;
  let contentHeight;
  try {
    const naturalWidth = Math.ceil(measure.getBoundingClientRect().width + horizontalPadding + 8);
    width = Math.min(naturalWidth, maxWidth);
    measure.style.width = `${Math.max(1, width - horizontalPadding)}px`;
    contentHeight = Math.ceil(measure.getBoundingClientRect().height + verticalPadding + 8);
  } finally {
    measure.remove();
  }
  const maxHeight = monitor
    ? Math.floor(monitor.workArea.size.height / scale - frameHeight)
    : Math.min(
        Math.floor(
          Number.isFinite(availableHeight) && availableHeight > 0
            ? availableHeight - frameHeight
            : window.innerHeight
        ),
        window.innerHeight
      );
  // A native resize can change wrapping (including by introducing a scrollbar).
  // Only use the rendered document once the WebView has reached the requested width.
  if (Math.abs(window.innerWidth - width) <= 1) {
    contentHeight = Math.max(
      contentHeight,
      Math.ceil(main.getBoundingClientRect().height + verticalPadding + 8)
    );
  }
  const height = Math.max(1, Math.min(contentHeight, maxHeight));
  if (Math.abs(window.innerWidth - width) > 1 || Math.abs(window.innerHeight - height) > 1) {
    await aboutWindow.setSize(
      new windowApi.LogicalSize(
        width + innerSize.width / scale - window.innerWidth,
        height + innerSize.height / scale - window.innerHeight
      )
    );
    if (monitor) {
      const [position, size] = await Promise.all([
        aboutWindow.outerPosition(),
        aboutWindow.outerSize(),
      ]);
      const { position: origin, size: available } = monitor.workArea;
      const x = Math.max(origin.x, Math.min(position.x, origin.x + available.width - size.width));
      const y = Math.max(origin.y, Math.min(position.y, origin.y + available.height - size.height));
      if (x !== position.x || y !== position.y) {
        await aboutWindow.setPosition(new windowApi.PhysicalPosition(x, y));
      }
    }
  }
}

function observeAboutSize() {
  let fitting = false;
  let pending = false;
  async function fit() {
    if (fitting) {
      pending = true;
      return;
    }
    fitting = true;
    do {
      pending = false;
      try {
        await fitAboutWindow();
      } catch (error) {
        console.error("[About] Failed to fit window:", redactDiagnostic(error));
      }
    } while (pending);
    fitting = false;
  }
  new ResizeObserver(fit).observe(document.querySelector("main"));
  window.addEventListener("resize", fit);
  document.fonts.ready.then(fit);
}
const colorScheme = window.matchMedia("(prefers-color-scheme: dark)");
function applyTheme() {
  document.documentElement.classList.toggle("dark", colorScheme.matches);
  document.documentElement.classList.toggle("light", !colorScheme.matches);
}

applyTheme();
colorScheme.addEventListener("change", applyTheme);

if (invoke) {
  invoke("get_i18n_bundle")
    .then((bundle) => {
      initI18n(bundle);
      applyTranslations();
    })
    .then(() => invoke("get_app_version"))
    .then((version) => {
      document.getElementById("version").textContent = t("desktop.about.version", version);
      document.getElementById("release-notes").href = releaseNotesUrl(version);
      document.title = t("desktop.mac_menu.about", t("desktop.about.app_name"));
    })
    .catch((error) => console.error("[About] Failed to load details:", redactDiagnostic(error)))
    .finally(observeAboutSize);
}
