(() => {
  let activeMessages = {};
  let fallbackMessages = {};

  function getByPath(obj, path) {
    return path.split(".").reduce((value, part) => value?.[part], obj);
  }

  function format(template, params) {
    return params.reduce(
      (value, param, index) => value.replaceAll(`{${index}}`, String(param)),
      template
    );
  }

  window.initI18n = function initI18n(bundle) {
    activeMessages = bundle?.messages || {};
    fallbackMessages = bundle?.fallback_messages || {};
    document.documentElement.lang = (bundle?.locale || "en").replace("_", "-");
  };

  window.t = function t(key, ...params) {
    const value = getByPath(activeMessages, key) ?? getByPath(fallbackMessages, key) ?? key;
    return typeof value === "string" ? format(value, params) : key;
  };

  window.applyTranslations = function applyTranslations(root = document) {
    root.querySelectorAll("[data-i18n]").forEach((el) => {
      el.textContent = window.t(el.dataset.i18n);
    });
    root.querySelectorAll("[data-i18n-aria-label]").forEach((el) => {
      el.setAttribute("aria-label", window.t(el.dataset.i18nAriaLabel));
    });
    root.querySelectorAll("[data-i18n-title]").forEach((el) => {
      el.setAttribute("title", window.t(el.dataset.i18nTitle));
    });
  };
})();
