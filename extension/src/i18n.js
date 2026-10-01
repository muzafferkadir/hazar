/* UI dili app'ten gelir (settings.language → chrome.storage.local). Yeni dil: locales/<kod>.js. */
(() => {
  let lang = "en";
  const listeners = [];
  const t = (key, ...args) => {
    const all = globalThis.HazarLocales || {};
    const value = all[lang]?.[key] ?? all.en?.[key] ?? key;
    return typeof value === "function" ? value(...args) : value;
  };
  const use = (value) => {
    const next = value && globalThis.HazarLocales?.[value] ? value : "en";
    if (next === lang) return;
    lang = next;
    listeners.forEach((fn) => fn(lang));
  };
  /** Statik HTML: data-i18n="key" textContent'i çevirir. */
  const apply = (root = document) => {
    // lang, CSS uppercase'i etkiler: "tr" altında "media" → "MEDİA" olur.
    if (root === document) document.documentElement.lang = lang;
    root.querySelectorAll("[data-i18n]").forEach((node) => { node.textContent = t(node.dataset.i18n); });
  };
  const ready = new Promise((resolve) => {
    try { chrome.storage.local.get({ language: "en" }, (stored) => { use(stored?.language); resolve(lang); }); }
    catch (_) { resolve(lang); }
  });
  try { chrome.storage.onChanged.addListener((changes) => { if (changes.language) use(changes.language.newValue); }); }
  catch (_) { /* storage unavailable */ }
  globalThis.HazarI18n = { t, apply, ready, onChange: (fn) => listeners.push(fn), get lang() { return lang; } };
})();
