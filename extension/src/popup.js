/** Hazar Integration — toolbar popup. */
"use strict";

const send = (message) =>
  new Promise((resolve) => {
    try {
      chrome.runtime.sendMessage(message, (response) => resolve(response || { ok: false }));
    } catch (_) {
      resolve({ ok: false });
    }
  });

/** Ham motor hatalarını kullanıcıya anlaşılır cümleye çevirir (app ile aynı mantık). */
function humanize(message) {
  if (/404/.test(message) && /\.m3u8|\.mpd/.test(message)) {
    return "bağlantının süresi dolmuş (oynatıcı tek kullanımlık token) — videoyu oynatıp tekrar gönder";
  }
  if (/not an HLS playlist/.test(message)) {
    return "stream tarayıcıda şifreli çözülüyor — indirilemiyor";
  }
  if (/SAMPLE-AES|widevine|playready/i.test(message)) return "DRM korumalı — indirilemiyor";
  if (/timed out|timeout/i.test(message)) return "zaman aşımı";
  return message;
}

function humanBytes(bytes) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = Number(bytes) || 0;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${unit === 0 ? value : value.toFixed(1)} ${units[unit]}`;
}

function el(tag, props = {}, children = []) {
  const node = document.createElement(tag);
  Object.assign(node, props);
  for (const child of children) node.append(child);
  return node;
}

async function activeTab() {
  const tabs = await chrome.tabs.query({ active: true, currentWindow: true });
  return tabs && tabs[0];
}

async function renderStatus() {
  const status = await send({ type: "status" });
  const dot = document.getElementById("dot");
  const label = document.getElementById("connection");
  const capture = document.getElementById("capture");
  const group = document.getElementById("group");

  dot.classList.toggle("on", !!status.connected);
  label.textContent = status.connected
    ? `${status.app ? status.app.app + " " + status.app.version : "bağlı"} · :${status.port}`
    : "Hazar açık değil";
  capture.checked = status.settings ? status.settings.capture_enabled !== false : true;
  group.checked = status.settings ? status.settings.group_hls !== false : true;
  capture.disabled = false;
  group.disabled = false;
  return status;
}

const rendered = new Map();

function changed(key, value) {
  const signature = JSON.stringify(value);
  if (rendered.get(key) === signature) return false;
  rendered.set(key, signature);
  return true;
}

async function renderCandidates() {
  const tab = await activeTab();
  if (!tab) return;

  const response = await send({ type: "candidates", tabId: tab.id });
  const candidates = (response.candidates || []).filter((item) => item.url);
  // Her 2 saniyede DOM'u baştan çizmek flicker yaratıyordu: içerik değişmediyse dur.
  if (!changed("candidates", candidates.map((item) => [item.url, item.kind, item.segments?.length ?? 0]))) {
    return;
  }

  const container = document.getElementById("candidates");
  container.textContent = "";
  if (!candidates.length) {
    container.append(el("div", { className: "empty", textContent: "Aday yok." }));
    return;
  }

  for (const candidate of candidates.slice(0, 8)) {
    const segments = candidate.segments || candidate.streams || [];
    const name = candidate.filename || candidate.url.split("/").pop() || candidate.url;
    const encrypted = Boolean(candidate.encrypted);
    const expired = Boolean(candidate.expired);
    // Yalnızca segment listesi varsa gönderilebilir: token tek kullanımlıksa app
    // manifest'i tekrar çekemez, ama sniff edilmiş segmentleri doğrudan indirebilir.
    const sendable = segments.length > 0 || (!encrypted && !expired);
    const detail = [
      candidate.kind,
      candidate.size ? humanBytes(candidate.size) : null,
      candidate.isManifest ? "playlist" : null,
      encrypted ? "tarayıcıda şifreli" : null,
      expired ? "bağlantı süresi dolmuş" : null,
      segments.length ? `${segments.length} segment` : null,
    ]
      .filter(Boolean)
      .join(" · ");

    const button = el("button", {
      className: "primary",
      textContent: sendable
        ? segments.length
          ? `Segmentleri indir (${segments.length})`
          : "Hazar'a gönder"
        : "indirilemez (şifreli/süresi dolmuş)",
    });
    if (!sendable) button.disabled = true;
    button.addEventListener("click", async () => {
      button.disabled = true;
      button.textContent = "gönderildi";
      const result = await send({
        type: "grab",
        url: candidate.url,
        kind: candidate.kind,
        pageUrl: candidate.pageUrl,
        pageTitle: tab.title,
        segments,
      });
      if (!result.ok) {
        button.textContent = result.connected === false ? "app kapalı" : "hata";
        button.disabled = false;
      } else {
        setTimeout(renderRecent, 400);
        // "gönderildi" durumunda takılı kalmasın.
        setTimeout(() => {
          button.textContent = "Hazar'a gönder";
          button.disabled = false;
        }, 4000);
      }
    });

    container.append(
      el("div", { className: "row" }, [
        el("div", { className: "meta" }, [
          el("div", { className: "name" }, [
            el("span", { className: "kind", textContent: candidate.kind || "file" }),
            document.createTextNode(name),
          ]),
          el("div", { className: "dim", textContent: detail }),
        ]),
        button,
      ]),
    );
  }
}

async function renderRecent() {
  const stored = await chrome.storage.local.get({ recent: [] });
  const recent = (stored.recent || []).filter((item) => item.state);
  if (!changed("recent", recent)) return;

  const container = document.getElementById("recent");
  container.textContent = "";
  if (!recent.length) {
    container.append(el("div", { className: "empty", textContent: "Henüz aktarım yok." }));
    return;
  }

  for (const item of recent.slice(0, 6)) {
    const total = Number(item.total) || 0;
    const written = Number(item.written) || 0;
    const ratio = total > 0 ? Math.min(100, (written / total) * 100) : 0;
    const line = el("div", { className: "row" }, [
      el("div", { className: "meta" }, [
        el("div", {
          className: "name",
          textContent: item.path ? item.path.split("/").pop() : item.id.slice(0, 8),
        }),
        el("div", {
          className: "dim",
          textContent:
            item.state === "failed"
              ? humanize(item.error || "hata")
              : `${item.state}${total ? ` · ${humanBytes(written)} / ${humanBytes(total)}` : ""}`,
        }),
        (() => {
          const bar = el("div", { className: "bar" });
          const fill = el("i");
          fill.style.width = `${ratio}%`;
          bar.append(fill);
          return bar;
        })(),
      ]),
    ]);
    container.append(line);
  }
}

async function refresh() {
  await renderStatus();
  await renderCandidates();
  await renderRecent();
}

document.getElementById("capture").addEventListener("change", async (event) => {
  await send({ type: "settings", patch: { capture_enabled: event.target.checked } });
});

document.getElementById("group").addEventListener("change", async (event) => {
  await send({ type: "settings", patch: { group_hls: event.target.checked } });
});

document.getElementById("reconnect").addEventListener("click", async () => {
  await send({ type: "reconnect" });
  setTimeout(refresh, 900);
});

document.getElementById("options").addEventListener("click", (event) => {
  event.preventDefault();
  chrome.runtime.openOptionsPage();
});

refresh();
setInterval(() => {
  renderStatus();
  renderRecent();
}, 2000);
