/**
 * Hazar Integration — recapture landing page.
 *
 * A `declarativeNetRequest` session rule redirects a `main_frame` navigation
 * here (exactly like IDM's `captured.html`). The navigation carries the fresh
 * session/cookies, so we hand the URL to Hazar again and then go back.
 */
"use strict";

const params = new URLSearchParams(location.search);
const ruleId = Number(params.get("rule")) || null;
const url = location.hash.length > 1 ? location.hash.slice(1) : null;

document.getElementById("url").textContent = url || "(adres okunamadı)";

function back() {
  if (history.length > 1) {
    history.back();
  } else {
    window.close();
  }
}

if (!url) {
  document.getElementById("status").textContent = "Adres bulunamadı.";
  back();
} else {
  chrome.runtime.sendMessage({ type: "grab", url, kind: undefined }, (response) => {
    const status = document.getElementById("status");
    status.textContent = response && response.ok
      ? "Hazar'a aktarıldı. Sayfaya dönülüyor…"
      : "Hazar'a ulaşılamadı — uygulama açık mı?";
    if (ruleId) chrome.runtime.sendMessage({ type: "clear-recapture-rule", ruleId });
    setTimeout(back, 1200);
  });
}
