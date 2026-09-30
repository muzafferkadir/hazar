/**
 * Hazar Integration — content script.
 *
 * Two jobs:
 *  - inject `page-hook.js` into the page context so we can see XHR/fetch
 *    traffic (HLS/DASH manifests and segments) that never reaches the extension
 *    APIs as a "download";
 *  - scan the DOM for playable media (`<video>`, `<audio>`, `<source>`,
 *    `<embed>`, `<object>`, og:video / twitter:player meta tags) and report it
 *    as a candidate to the background worker.
 */
(() => {
  const SOURCE = "hazar-page-hook";
  const seen = new Set();
  let scanTimer = null;

  function injectPageHook() {
    try {
      const script = document.createElement("script");
      script.src = chrome.runtime.getURL("page-hook.js");
      script.async = false;
      script.onload = () => script.remove();
      (document.head || document.documentElement).appendChild(script);
    } catch (error) {
      /* CSP or a detached document — sniffing still works via webRequest */
    }
  }

  /** Base64'e çevir (büyük gövdelerde parça parça, stack taşmasın). */
  function toBase64(buffer) {
    const bytes = new Uint8Array(buffer);
    let binary = "";
    const chunk = 0x8000;
    for (let index = 0; index < bytes.length; index += chunk) {
      binary += String.fromCharCode.apply(null, bytes.subarray(index, index + chunk));
    }
    return btoa(binary);
  }

  // Background, segmenti oynatıcının kendi frame'inde indirmek ister: burada
  // fetch çalıştığı için isteğin Referer'ı ve çerezleri oynatıcınınkiyle aynı olur.
  chrome.runtime.onMessage.addListener((message, _sender, respond) => {
    if (!message || message.type !== "fetch_bytes") return false;
    (async () => {
      try {
        const response = await fetch(message.url, { credentials: "include" });
        if (!response.ok) {
          respond({ ok: false, status: response.status });
          return;
        }
        const buffer = await response.arrayBuffer();
        respond({ ok: true, base64: toBase64(buffer), bytes: buffer.byteLength });
      } catch (error) {
        respond({ ok: false, error: String(error) });
      }
    })();
    return true; // async yanıt
  });

  function send(message) {
    try {
      chrome.runtime.sendMessage(message);
    } catch (_) {
      /* extension context invalidated */
    }
  }

  window.addEventListener("message", (event) => {
    if (event.source !== window) return;
    const data = event.data;
    if (!data || data.source !== SOURCE) return;
    if (data.kind === "segment" || data.kind === "manifest") {
      send({ type: "page-hook", payload: { kind: data.kind, url: data.url, mime: data.mime } });
    } else if (data.kind === "manifest-body") {
      send({
        type: "page-hook",
        payload: { kind: "manifest-body", url: data.url, mime: data.mime, body: data.body },
      });
    }
  });

  function absolute(url) {
    try {
      return new URL(url, location.href).toString();
    } catch (_) {
      return null;
    }
  }

  function reportMedia(url, extra) {
    const absoluteUrl = absolute(url);
    if (!absoluteUrl || !/^https?:/i.test(absoluteUrl) || seen.has(absoluteUrl)) return;
    seen.add(absoluteUrl);
    const kind = /\.(m3u8|m3u)(\?|$)/i.test(absoluteUrl)
      ? "hls"
      : /\.mpd(\?|$)/i.test(absoluteUrl)
        ? "dash"
        : "media";
    send({ type: "media-candidate", payload: { url: absoluteUrl, kind, ...extra } });
  }

  function scan() {
    scanTimer = null;

    for (const element of document.querySelectorAll("video, audio, source, embed, object")) {
      const url =
        element.currentSrc ||
        element.src ||
        element.getAttribute("data-src") ||
        element.getAttribute("src");
      if (url) {
        reportMedia(url, {
          mime: element.type || null,
          label: element.tagName.toLowerCase(),
          filename: element.getAttribute("title") || null,
        });
      }
    }

    for (const meta of document.querySelectorAll("meta[property], meta[name]")) {
      const key = (meta.getAttribute("property") || meta.getAttribute("name") || "").toLowerCase();
      if (!/video|audio|player|stream|content_url/.test(key)) continue;
      const content = meta.getAttribute("content") || "";
      if (!/^https?:/i.test(content)) continue;
      reportMedia(content, { label: key, mime: null });
    }
  }

  function scheduleScan() {
    if (scanTimer) return;
    scanTimer = setTimeout(scan, 400);
  }

  injectPageHook();

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", scan, { once: true });
  } else {
    scan();
  }

  try {
    const observer = new MutationObserver(scheduleScan);
    observer.observe(document.documentElement || document, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["src", "content"],
    });
  } catch (_) {
    /* ignore */
  }

  // Media elements often only learn their real source after `loadedmetadata`.
  document.addEventListener(
    "loadedmetadata",
    (event) => {
      const target = event.target;
      if (target && (target.currentSrc || target.src)) {
        reportMedia(target.currentSrc || target.src, {
          mime: target.type || null,
          label: target.tagName ? target.tagName.toLowerCase() : null,
        });
      }
    },
    true,
  );
})();
