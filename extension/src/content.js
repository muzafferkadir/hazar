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
  const panels = new Map();
  let positioning = false;
  let querying = false;
  const t = globalThis.HazarI18n?.t || (key => key);
  globalThis.HazarI18n?.onChange(() => {
    for (const panel of panels.values()) { panel.fingerprint = null; panel.button.title = t("panelDownload"); panel.close.title = t("panelClose"); panel.rescan.title = t("rescan"); }
    void refreshPanels();
  });

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
    if (message?.type === "collect_download_links") {
      const links = [];
      const unique = new Set();
      const selection = window.getSelection();
      for (const anchor of document.querySelectorAll("a[href]")) {
        if (message.selected && (!selection || !selection.containsNode(anchor, true))) continue;
        try {
          const url = new URL(anchor.href);
          if (!["http:", "https:"].includes(url.protocol) || unique.has(url.href)) continue;
          if (!anchor.hasAttribute("download") && !/\.(zip|rar|7z|tar|gz|iso|dmg|exe|msi|pdf|epub|mp4|mkv|webm|mov|mp3|m4a|flac|wav|m3u8|mpd)(?:$)/i.test(url.pathname)) continue;
          unique.add(url.href);
          links.push({ url: url.href, label: anchor.textContent.trim().slice(0, 160) || null });
          if (links.length >= 200) break;
        } catch (_) {}
      }
      respond({ links }); return false;
    }
    if (!message || message.type !== "fetch_bytes") return false;
    (async () => {
      try {
        const range = message.range;
        if (range && (!Number.isSafeInteger(range.start) || !Number.isSafeInteger(range.end) || range.start < 0 || range.end < range.start)) throw new Error("Geçersiz Range");
        const headers = range ? { Range: `bytes=${range.start}-${range.end}` } : {};
        let response = await fetch(message.url, { credentials: "same-origin", headers });
        // Match normal player requests; wildcard CORS CDNs reject include.
        // Cookie-gated cross-origin servers can still opt into credentialed retry.
        if ([401, 403].includes(response.status) && new URL(message.url, location.href).origin !== location.origin) {
          response = await fetch(message.url, { credentials: "include", headers });
        }
        if (!response.ok) {
          respond({ ok: false, status: response.status });
          return;
        }
        let buffer = await response.arrayBuffer();
        if (range) {
          if (response.status === 200) {
            if (buffer.byteLength <= range.end) throw new Error("Range response eksik");
            buffer = buffer.slice(range.start, range.end + 1);
          } else if (response.status === 206) {
            const actual = /^bytes (\d+)-(\d+)\//i.exec(response.headers.get("content-range") || "");
            if (actual && (Number(actual[1]) !== range.start || Number(actual[2]) !== range.end)) throw new Error("Content-Range eşleşmiyor");
          } else throw new Error("Range response status geçersiz");
          if (buffer.byteLength !== range.end - range.start + 1) throw new Error("Range response boyutu eşleşmiyor");
        }
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
    if (data.kind === "manifest-body" || data.kind === "manifest") void refreshPanels();
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
    syncPanels();

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

  function rpc(message) {
    return new Promise(resolve => {
      try {
        chrome.runtime.sendMessage(message, result => {
          const error = chrome.runtime.lastError;
          resolve(error ? { ok: false, error: t("reloadExtension") } : result || { ok: false });
        });
      } catch (_) { resolve({ ok: false, error: t("reloadExtension") }); }
    });
  }

  function createPanel(video) {
    const host = document.createElement("div");
    host.dataset.hazarPanel = "";
    host.style.cssText = "position:fixed;z-index:2147483647;pointer-events:none;display:none";
    const root = host.attachShadow({ mode: "closed" });
    root.innerHTML = `<style>
      :host{all:initial}*{box-sizing:border-box}
      .panel{font:13px -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;color:#fff;pointer-events:auto;display:flex;flex-direction:column;align-items:flex-end}
      button{font:inherit;cursor:pointer;color:inherit;border:0;background:transparent;padding:0;line-height:1.3}
      button:disabled{cursor:wait}button:focus-visible{outline:2px solid #7FD0FF;outline-offset:2px}
      .logo{width:24px;height:24px;border-radius:6px;display:grid;place-items:center;opacity:.45;box-shadow:0 1px 6px rgba(0,0,0,.35);transition:opacity .15s,transform .15s}
      .logo:hover,.panel.open .logo{opacity:1;transform:scale(1.06)}.logo img{width:24px;height:24px;display:block}
      .card{margin-top:6px;min-width:220px;max-width:300px;background:rgba(6,31,66,.95);backdrop-filter:blur(10px);border:1px solid rgba(127,208,255,.25);border-radius:10px;box-shadow:0 6px 20px rgba(0,0,0,.5);overflow:hidden}
      .card[hidden]{display:none}
      .head{display:flex;align-items:center;justify-content:space-between;padding:7px 8px 7px 11px;font-weight:600;border-bottom:1px solid rgba(127,208,255,.15)}
      .head-actions{display:flex;gap:2px}.rescan,.close{width:22px;height:22px;border-radius:6px;font-size:15px;line-height:1;opacity:.8}.rescan:hover,.close:hover{background:rgba(255,255,255,.12);opacity:1}
      .menu{padding:4px;max-height:260px;overflow:auto}
      .menu button{display:flex;align-items:center;gap:8px;width:100%;text-align:left;padding:7px 8px;border-radius:7px}
      .menu button:hover{background:rgba(127,208,255,.14)}
      .q{flex:none;min-width:44px;text-align:center;font-size:11px;font-weight:700;padding:2px 6px;border-radius:5px;background:#0A5A94}
      .ytdlp .q{background:transparent;border:1px solid #7FD0FF;color:#8FDBFF}
      .name{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
      .status{padding:7px 11px;border-top:1px solid rgba(127,208,255,.15);font-size:12px;line-height:1.4;overflow-wrap:anywhere}.status:empty{display:none}
    </style><div class="panel"><button class="logo" type="button" aria-expanded="false"><img alt="Hazar"></button><div class="card" hidden><div class="head"><span>Hazar</span><span class="head-actions"><button class="rescan" type="button">↻</button><button class="close" type="button">×</button></span></div><div class="menu"></div><div class="status" role="status"></div></div></div>`;
    const panel = { video, host, button: root.querySelector(".logo"), card: root.querySelector(".card"),
      root: root.querySelector(".panel"), close: root.querySelector(".close"),
      menu: root.querySelector(".menu"), status: root.querySelector(".status"), candidates: [], busy: false };
    root.querySelector(".logo img").src = chrome.runtime.getURL("icons/32x32.png");
    panel.button.title = panel.button.ariaLabel = t("panelDownload");
    panel.close.title = panel.close.ariaLabel = t("panelClose");
    const setOpen = open => { panel.card.hidden = !open; panel.root.classList.toggle("open", open); panel.button.setAttribute("aria-expanded", String(open)); positionPanels(); };
    panel.setOpen = setOpen;
    // Liste sadece × ile kapanır; video tıklamaları paneli kapatmaz.
    panel.close.addEventListener("click", () => setOpen(false));
    panel.rescan = root.querySelector(".rescan");
    panel.rescan.title = panel.rescan.ariaLabel = t("rescan");
    panel.rescan.addEventListener("click", async () => {
      panel.status.textContent = t("analyzing");
      await rpc({ type: "rescan", pageUrl: location.href });
      panel.fingerprint = null; await refreshPanels();
      setTimeout(() => { panel.fingerprint = null; void refreshPanels().then(() => { if (panel.status.textContent === t("analyzing")) panel.status.textContent = ""; }); }, 1500);
    });
    root.addEventListener("pointerdown", event => event.stopPropagation());
    root.addEventListener("click", event => event.stopPropagation());
    panel.button.addEventListener("click", () => setOpen(panel.card.hidden));
    root.addEventListener("keydown", event => { if (event.key === "Escape") setOpen(false); });
    panel.resize = new ResizeObserver(positionPanels);
    panel.resize.observe(video);
    (document.body || document.documentElement).appendChild(host);
    return panel;
  }

  async function download(panel, candidate) {
    panel.busy = true;
    panel.menu.querySelectorAll("button").forEach(b => { b.disabled = true; });
    panel.status.textContent = t("panelSending");
    const result = await rpc({ type: "grab", url: candidate.url, kind: candidate.kind,
      extractor: candidate.extractor, height: candidate.height, filename: candidate.filename, pageTitle: document.title });
    panel.busy = false;
    panel.menu.querySelectorAll("button").forEach(b => { b.disabled = false; });
    panel.status.textContent = result.ok ? t("panelSent") : (result.error || t("openApp"));
    if (result.ok) setTimeout(() => { if (panel.status.textContent === t("panelSent")) { panel.status.textContent = ""; panel.setOpen(false); } }, 1800);
    positionPanels();
  }

  function syncPanels() {
    for (const [video, panel] of panels) {
      if (!video.isConnected) { panel.resize.disconnect(); panel.host.remove(); panels.delete(video); }
    }
    for (const video of document.querySelectorAll("video")) {
      if (!panels.has(video)) panels.set(video, createPanel(video));
    }
    positionPanels();
    void refreshPanels();
  }

  function positionPanels() {
    if (positioning) return;
    positioning = true;
    requestAnimationFrame(() => {
      positioning = false;
      for (const panel of panels.values()) {
        const rect = panel.video.getBoundingClientRect();
        const style = getComputedStyle(panel.video);
        const visible = panel.candidates.length && rect.width >= 140 && rect.height >= 80
          && rect.bottom > 0 && rect.right > 0 && rect.top < innerHeight && rect.left < innerWidth
          && style.visibility !== "hidden" && style.display !== "none" && Number(style.opacity) !== 0;
        const fullscreen = document.fullscreenElement;
        if (!visible || (fullscreen && (fullscreen.tagName === "VIDEO" || !fullscreen.contains(panel.video)))) {
          panel.host.style.display = "none"; continue;
        }
        const parent = fullscreen || document.body || document.documentElement;
        if (panel.host.parentNode !== parent) parent.appendChild(panel.host);
        panel.host.style.display = "block";
        const width = panel.host.getBoundingClientRect().width;
        panel.host.style.left = `${Math.max(4, Math.min(innerWidth - width - 4, rect.right - width - 8))}px`;
        panel.host.style.top = `${Math.max(4, rect.top + 8)}px`;
      }
    });
  }

  async function refreshPanels() {
    if (querying || !panels.size || document.hidden) return;
    querying = true;
    try {
      for (const panel of panels.values()) {
        if (!panel.video.isConnected) continue;
        const src = panel.video.currentSrc || panel.video.src;
        const result = await rpc({ type: "video_candidates", mediaUrl: src, pageUrl: location.href });
        let candidates = result.candidates || [];
        const exact = candidates.filter(c => c.url === src);
        if (exact.length) candidates = [...exact, ...candidates.filter(c => c.extractor === "ytdlp")];
        else if (panels.size > 1) {
          // Network-only blob candidates cannot reliably identify several players.
          const playing = [...panels.keys()].filter(v => !v.paused && !v.ended);
          if (playing.length !== 1 || playing[0] !== panel.video) candidates = [];
        }
        if (candidates.length) {
          for (const track of panel.video.querySelectorAll('track[src]')) {
            if (!['subtitles', 'captions'].includes(track.kind) || !/^https?:/i.test(track.src)) continue;
            candidates.push({ url: track.src, kind: 'file', label: t("subtitle", track.label || track.srclang || 'VTT') });
          }
        }
        // Doğrudan video dosyasında çözünürlük oynatıcıdan okunur.
        candidates = candidates.map(c => c.label === "Video" && !c.height && panel.video.videoHeight ? { ...c, height: panel.video.videoHeight } : c);
        const fingerprint = JSON.stringify([HazarI18n.lang, candidates]);
        if (panel.fingerprint !== fingerprint) {
          panel.fingerprint = fingerprint;
          panel.candidates = candidates;
          panel.menu.replaceChildren();
          for (const candidate of candidates) {
            const height = candidate.height || Number(/^(\d{3,4})p$/.exec(candidate.label || "")?.[1]) || 0;
            const button = document.createElement("button");
            button.type = "button";
            button.classList.toggle("ytdlp", candidate.extractor === "ytdlp");
            const q = document.createElement("span"); q.className = "q";
            q.textContent = height ? `${height}p` : candidate.extractor === "ytdlp" ? "yt-dlp" : (candidate.kind || "file").toUpperCase();
            const name = document.createElement("span"); name.className = "name";
            name.textContent = String(candidate.label || "").replace(/^yt-dlp · (\d{3,4}p · )?/, "").replace(/^\d{3,4}p$/, "Video") || "Video";
            name.title = name.textContent;
            button.append(q, name);
            button.addEventListener("click", () => { if (!panel.busy) void download(panel, candidate); });
            panel.menu.appendChild(button);
          }
        }
      }
    } finally { querying = false; positionPanels(); }
  }

  window.addEventListener("scroll", positionPanels, { passive: true, capture: true });
  window.addEventListener("resize", positionPanels, { passive: true });
  document.addEventListener("fullscreenchange", positionPanels);
  document.addEventListener("play", () => { void refreshPanels(); }, true);
  document.addEventListener("visibilitychange", () => { void refreshPanels(); });
  let panelTimer = setInterval(syncPanels, 2000);
  window.addEventListener("pagehide", () => clearInterval(panelTimer));
  window.addEventListener("pageshow", event => { if (event.persisted) { panelTimer = setInterval(syncPanels, 2000); syncPanels(); } });

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
    const observer = new MutationObserver(records => {
      if (records.some(record => !record.target.closest?.('[data-hazar-panel]')
        && (record.type === 'attributes' || [...record.addedNodes, ...record.removedNodes]
          .some(node => node.nodeType === 1 && !node.hasAttribute?.('data-hazar-panel'))))) scheduleScan();
    });
    observer.observe(document.documentElement || document, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["src", "content", "style", "class", "hidden"],
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
