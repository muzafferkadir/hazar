/** Hazar Integration — toolbar popup. */
"use strict";

const t = HazarI18n.t;

const send = (message) =>
  new Promise((resolve) => {
    try {
      chrome.runtime.sendMessage(message, (response) => resolve(response || { ok: false, error: chrome.runtime.lastError?.message || t("noResponse") }));
    } catch (_) {
      resolve({ ok: false });
    }
  });

/** Ham motor hatalarını kullanıcıya anlaşılır cümleye çevirir (lib.js ile ortak). */
function humanize(message) {
  return HazarLib.humanizeError(message);
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
  const extensionVersion = chrome.runtime.getManifest().version;
  label.textContent = status.connected
    ? `${status.app ? status.app.app + " " + status.app.version : t("connected")} · :${status.port} · ext ${extensionVersion}`
    : t("appNotRunning", extensionVersion);
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
  if (!changed("candidates", [tab.id, response.extractor, candidates.map((item) => [item.url, item.kind, item.extractor, item.segments?.length ?? 0])])) {
    return;
  }

  const container = document.getElementById("candidates");
  container.textContent = "";
  if (response.extractor?.pending) container.append(el("div", { className: "dim", textContent: t("analyzing") }));
  if (response.extractor?.error) {
    const retry = el("button", { textContent: t("reanalyze") });
    retry.addEventListener("click", async () => {
      retry.disabled = true;
      const result = await send({ type: "retry_extractor", tabId: tab.id });
      if (!result.ok) retry.textContent = result.error;
      rendered.delete("candidates"); await renderCandidates();
    });
    container.append(el("div", { className: "extractor-error" }, [
      el("div", { textContent: `yt-dlp · ${response.extractor.error.message}` }), retry]));
  }
  if (!candidates.length) {
    container.append(el("div", { className: "empty", textContent: t("noCandidates") }));
    return;
  }

  for (const candidate of [...candidates.filter(c => !c.extractor).slice(0, 8), ...candidates.filter(c => c.extractor)]) {
    const segments = candidate.segments || candidate.streams || [];
    const name = candidate.filename || candidate.url.split("/").pop() || candidate.url;
    const encrypted = Boolean(candidate.encrypted);
    const expired = Boolean(candidate.expired);
    // Yalnızca segment listesi varsa gönderilebilir: token tek kullanımlıksa app
    // manifest'i tekrar çekemez, ama sniff edilmiş segmentleri doğrudan indirebilir.
    const sendable = segments.length > 0 || (!encrypted && !expired);
    const detail = [
      candidate.height ? `${candidate.height}p` : null,
      candidate.kind,
      candidate.size ? humanBytes(candidate.size) : null,
      candidate.isManifest ? t("playlist") : null,
      encrypted ? t("encrypted") : null,
      expired ? t("expired") : null,
      segments.length
        ? (candidate.isMaster ? t("variants", segments.length) : t("segments", segments.length))
        : null,
    ]
      .filter(Boolean)
      .join(" · ");

    const errorMessage = el("div", { className: "dim", hidden: true, role: "alert" });
    const button = el("button", {
      className: "primary",
      textContent: sendable
        ? segments.length
          ? t("downloadSegments", segments.length)
          : t("send")
        : t("notDownloadable"),
    });
    if (!sendable) button.disabled = true;

    // Segmentler elimizdeyse oynatıcının oturumunda indirip app'e aktarırız.
    const downloadLabel = candidate.isMaster ? t("variants", segments.length) : t("segments", segments.length);
    const frameButton = segments.length
      ? el("button", { className: "primary", textContent: t("downloadInPage", downloadLabel) })
      : null;
    if (frameButton) {
      frameButton.addEventListener("click", async () => {
        frameButton.disabled = true;
        frameButton.textContent = t("downloading");
        const result = await send({
          type: "save_stream",
          tabId: tab.id,
          frameId: candidate.frameId ?? null,
          url: candidate.url,
          segments,
          filename: candidate.filename || "stream.ts",
        });
        frameButton.textContent = result.ok
          ? t("transferred", result.segments ?? segments.length)
          : t("failed");
        if (!result.ok) {
          frameButton.disabled = false;
          frameButton.textContent = t("downloadInPage", downloadLabel);
        }
        setTimeout(renderRecent, 500);
      });
    }
    button.addEventListener("click", async () => {
      button.disabled = true;
      button.textContent = t("sent");
      errorMessage.hidden = true;
      const result = await send({
        type: "grab",
        tabId: tab.id,
        url: candidate.url,
        kind: candidate.kind,
        extractor: candidate.extractor,
        height: candidate.height,
        filename: candidate.filename,
        pageUrl: candidate.pageUrl,
        pageTitle: tab.title,
        segments,
      });
      if (!result.ok) {
        button.textContent = result.connected === false ? t("appClosed") : t("error");
        errorMessage.textContent = humanize(result.error || t("rejected"));
        errorMessage.hidden = false;
        button.disabled = false;
      } else {
        setTimeout(renderRecent, 400);
        // "gönderildi" durumunda takılı kalmasın.
        setTimeout(() => {
          button.textContent = t("send");
          button.disabled = false;
        }, 4000);
      }
    });

    container.append(
      el("div", { className: candidate.extractor === "ytdlp" ? "row ytdlp" : "row" }, [
        el("div", { className: "meta" }, [
          el("div", { className: "name" }, [
            el("span", { className: "kind", textContent: candidate.extractor === "ytdlp" ? "yt-dlp" : candidate.kind || "file" }),
            document.createTextNode(name),
          ]),
          el("div", { className: "dim", textContent: detail }),
          errorMessage,
        ]),
        el("div", { className: "row-actions" }, [frameButton, button].filter(Boolean)),
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
    container.append(el("div", { className: "empty", textContent: t("noRecent") }));
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
              ? humanize(item.error || t("error"))
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

document.getElementById("clear-recent").addEventListener("click", async () => {
  await chrome.storage.local.set({ recent: [] });
  rendered.delete("recent");
  await renderRecent();
});

document.getElementById("options").addEventListener("click", (event) => {
  event.preventDefault();
  chrome.runtime.openOptionsPage();
});

HazarI18n.onChange(() => { HazarI18n.apply(); rendered.clear(); refresh(); });
HazarI18n.ready.then(() => { HazarI18n.apply(); refresh(); });
setInterval(() => {
  renderStatus();
  renderRecent();
  renderCandidates();
}, 2000);
