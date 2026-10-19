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
    .catch((error) => console.error("[About] Failed to load details:", redactDiagnostic(error)));
}
