/**
 * Hazar Integration — background service worker (Chrome MV3) / event page (Firefox MV3).
 *
 * Responsibilities
 *  1. keep a WebSocket to the Hazar app (127.0.0.1, sub-protocol hazar.v1)
 *  2. observe downloads (downloads API + webRequest) and hand them over
 *  3. sniff HLS/DASH manifests and segment streams
 *  4. mirror app progress into the toolbar badge and storage for the popup
 *
 * Protocol: named JSON (see crates/hazar-localapi). Deliberately *not* IDM's
 * positional opcode arrays.
 */
"use strict";

if (typeof importScripts === "function" && typeof HazarLib === "undefined") {
  try {
    importScripts("lib.js", "locales/en.js", "locales/tr.js", "i18n.js");
  } catch (error) {
    console.error("hazar: could not load lib.js", error);
  }
}
const Lib = globalThis.HazarLib;
const t = (...args) => globalThis.HazarI18n?.t(...args) ?? args[0];

const PROTOCOL = 1;
const SUBPROTOCOL = "hazar.v1";
const RECONNECT_MIN = 1000;
const RECONNECT_MAX = 30000;
const PORT_PROBE_TIMEOUT = 1500;
const ACK_TIMEOUT = 2500;
const RECENT_LIMIT = 30;

const DEFAULT_SETTINGS = {
  capture_enabled: true,
  capture_manifests: true,
  group_hls: true,
  min_size_bytes: 512 * 1024,
  excluded_hosts: [],
  deny_patterns: [],
  ytdlp_proxy: "",
  ytdlp_source_address: "",
  ytdlp_impersonate_hosts: [],
  ports: [8722, 8723, 8724, 8725, 8726, 8727, 8728, 8729, 8730],
};

const state = {
  settings: { ...DEFAULT_SETTINGS },
  socket: null,
  session: null,
  port: null,
  appInfo: null,
  features: [],
  connecting: false,
  reconnectDelay: RECONNECT_MIN,
  reconnectTimer: null,
  probeTimer: null,
  portIndex: 0,
  requests: new Map(), // requestId -> record
  byUrl: new Map(), // url -> record (last response seen)
  candidates: new Map(), // tabId -> Map(key -> candidate)
  segments: new Map(), // tabId -> Map(bucket -> { urls:Set, manifestUrl, pageUrl })
  manifests: new Set(),
  pending: new Map(), // grab id -> { resolve, timer }
  // grab id -> app 403 verirse oynatıcı oturumunda indirilecek segmentler
  grabFallbacks: new Map(),
  stats: { active: 0, done: 0, failed: 0 },
  counters: { beforeRequest: 0, beforeSendHeaders: 0, headersReceived: 0, manifests: 0, candidates: 0, errors: 0 },
  ping: null,
  lastGoodPort: null,
  log: [], // ring buffer, readable from the test harness via __hazarDebug()
};

/** Append to the debug ring buffer (also mirrored to the service worker console). */
function debug(...parts) {
  const line = parts.join(" ");
  state.log.push(`${new Date().toISOString().slice(11, 23)} ${line}`);
  if (state.log.length > 80) state.log.splice(0, state.log.length - 80);
  console.debug(`hazar: ${line}`);
}

/** Introspection hook: the capture test matrix attaches to the service worker
 *  and reads this to explain why a download was or was not taken over. */
globalThis.__hazarDebug = () => ({
  connected: isConnected(),
  port: state.port,
  app: state.appInfo,
  settings: state.settings,
  stats: state.stats,
  seenUrls: state.byUrl.size,
  recentUrls: [...state.byUrl.keys()].slice(-8),
  inFlight: state.requests.size,
  pendingGrabs: state.pending.size,
  libLoaded: typeof Lib !== "undefined",
  counters: state.counters,
  tabs: [...state.candidates.keys()],
  segmentBuckets: [...state.segments.entries()].map(([tab, buckets]) => [
    Number(tab),
    [...buckets.entries()].map(([key, entry]) => [key, entry.urls.size]),
  ]),
  log: state.log.slice(-25),
});

// ---------------------------------------------------------------------------
// settings / storage
// ---------------------------------------------------------------------------

function loadSettings() {
  return new Promise((resolve) => {
    chrome.storage.local.get(DEFAULT_SETTINGS, (stored) => {
      state.settings = { ...DEFAULT_SETTINGS, ...(stored || {}) };
      state.lastGoodPort = stored && stored.lastPort ? stored.lastPort : null;
      resolve(state.settings);
    });
  });
}

function saveSettings(patch) {
  state.settings = { ...state.settings, ...patch };
  if (["ytdlp_proxy", "ytdlp_source_address", "ytdlp_impersonate_hosts"].some(key => key in patch)) {
    extractionCache.clear();
    for (const [host, cooldown] of extractorCooldowns) if (cooldown.error.code !== "rate_limit") extractorCooldowns.delete(host);
  }
  return new Promise((resolve) => chrome.storage.local.set(state.settings, resolve));
}

function updateRecent(entry) {
  chrome.storage.local.get({ recent: [] }, (stored) => {
    const recent = (stored.recent || []).filter((item) => item.id !== entry.id);
    recent.unshift(entry);
    chrome.storage.local.set({ recent: recent.slice(0, RECENT_LIMIT) });
  });
}

function setBadge() {
  const text = state.stats.active > 0 ? String(state.stats.active) : "";
  try {
    chrome.action.setBadgeText({ text });
    chrome.action.setBadgeBackgroundColor({ color: state.stats.failed > 0 ? "#ff453a" : "#0A5A94" });
    chrome.action.setTitle({
      title: state.port
        ? `Hazar Download Manager — app listening on 127.0.0.1:${state.port}`
        : "Hazar Download Manager — app is not running",
    });
  } catch (_) {
    /* action may be unavailable in rare contexts */
  }
}

/** Sniffed media candidates for every tab (the browser layer of the test
 *  matrix reads this through CDP; the popup uses it for its list). */
globalThis.__hazarCandidates = async (tabIdOrUrl) => {
  let wanted;
  if (typeof tabIdOrUrl === "number") {
    wanted = [`${tabIdOrUrl}`];
  } else if (typeof tabIdOrUrl === "string" && tabIdOrUrl) {
    // Resolve the tab that is showing this URL, so rows never read each
    // other's sniffed traffic.
    wanted = [];
    try {
      const tabs = await chrome.tabs.query({});
      const exact = tabs.find((tab) => tab && tab.url === tabIdOrUrl);
      const prefix = exact || tabs.find((tab) => tab && tab.url && tab.url.startsWith(tabIdOrUrl));
      if (prefix) wanted = [`${prefix.id}`];
    } catch (_) {
      /* tabs permission missing or worker restarting */
    }
  } else {
    wanted = [...state.candidates.keys()];
  }
  const out = [];
  for (const key of wanted) {
    const map = state.candidates.get(key);
    if (!map) continue;
    const buckets = state.segments.get(key);
    for (const candidate of map.values()) {
      const sniffed = buckets ? [...buckets.values()].flatMap((entry) => [...entry.urls]) : [];
      const parsed = Array.isArray(candidate.segments) ? candidate.segments : null;
      out.push({
        url: candidate.url,
        kind: candidate.kind || "file",
        mime: candidate.mime || null,
        size: candidate.size || null,
        isManifest: !!candidate.isManifest,
        isMaster:
          typeof candidate.manifest === "string" && candidate.manifest.includes("#EXT-X-STREAM-INF"),
        pageUrl: candidate.pageUrl || null,
        frameUrl: candidate.frameUrl || null,
        frameId: typeof candidate.frameId === "number" ? candidate.frameId : null,
        encrypted: !!candidate.encrypted,
        expired: !!candidate.expired,
        status: candidate.status || null,
        segments: parsed && parsed.length ? Lib.preferSniffedSegments(parsed, sniffed) : null,
        tabId: Number(key),
      });
    }
  }
  return out;
};

// ---------------------------------------------------------------------------
// websocket to the app
// ---------------------------------------------------------------------------

function isConnected() {
  return !!state.socket && state.socket.readyState === WebSocket.OPEN && !!state.session;
}

function connect() {
  if (isConnected() || state.connecting) {
    debug(`connect skipped (connected=${isConnected()} connecting=${state.connecting})`);
    return;
  }
  const configured = state.settings.ports || DEFAULT_SETTINGS.ports;
  // Önce en son başarılı portu dene, sonra sırayla diğerleri.
  const ports = state.lastGoodPort && configured.includes(state.lastGoodPort)
    ? [state.lastGoodPort, ...configured.filter((value) => value !== state.lastGoodPort)]
    : configured;
  if (!ports.length) return;

  state.connecting = true;
  const port = ports[state.portIndex % ports.length];

  let socket;
  try {
    socket = new WebSocket(`ws://127.0.0.1:${port}/hazar`, SUBPROTOCOL);
  } catch (error) {
    debug(`WebSocket constructor failed: ${error}`);
    state.connecting = false;
    return scheduleReconnect();
  }

  state.probeTimer = setTimeout(() => {
    if (socket.readyState !== WebSocket.OPEN) {
      try {
        socket.close();
      } catch (_) {
        /* ignore */
      }
    }
  }, PORT_PROBE_TIMEOUT);

  socket.addEventListener("open", () => {
    clearTimeout(state.probeTimer);
    debug(`socket open on :${port}`);
    state.socket = socket;
    state.connecting = false;
    state.port = port;
    state.lastGoodPort = port;
    state.reconnectDelay = RECONNECT_MIN;
    // `rawSend`: the session only exists after the app answers this frame.
    rawSend({
      type: "hello",
      protocol: PROTOCOL,
      client: clientName(),
      extension_id: chrome.runtime.id || null,
      version: chrome.runtime.getManifest().version,
    });
  });

  socket.addEventListener("message", (event) => {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch (error) {
      console.warn("hazar: bad message from app", error);
      return;
    }
    handleAppMessage(message);
  });

  const closed = () => {
    clearTimeout(state.probeTimer);
    stopKeepalive();
    if (state.socket === socket) {
      state.socket = null;
      state.session = null;
      state.port = null;
      state.appInfo = null;
      state.features = [];
    }
    state.connecting = false;
    state.portIndex += 1;
    scheduleReconnect();
    setBadge();
  };

  socket.addEventListener("close", (event) => {
    debug(`socket closed :${port} code=${event.code} clean=${event.wasClean}`);
    closed();
  });
  socket.addEventListener("error", () => {
    debug(`socket error on :${port}`);
    try {
      socket.close();
    } catch (_) {
      /* ignore */
    }
  });
}

function scheduleReconnect() {
  if (state.reconnectTimer) return;
  const delay = state.reconnectDelay;
  state.reconnectDelay = Math.min(RECONNECT_MAX, state.reconnectDelay * 2);
  state.reconnectTimer = setTimeout(() => {
    state.reconnectTimer = null;
    connect();
  }, delay);
}

function clientName() {
  const ua = navigator.userAgent || "";
  if (/Firefox\//.test(ua)) return "firefox";
  if (/Edg\//.test(ua)) return "edge";
  return "chrome";
}

/** Write to the socket without requiring a session (handshake frames). */
function rawSend(message) {
  if (!state.socket || state.socket.readyState !== WebSocket.OPEN) return false;
  try {
    state.socket.send(JSON.stringify(message));
    return true;
  } catch (error) {
    console.warn("hazar: send failed", error);
    return false;
  }
}

/** Write to the socket; requires a completed handshake. */
function send(message) {
  if (!isConnected()) return false;
  return rawSend(message);
}

function startKeepalive() {
  stopKeepalive();
  // MV3 service workers are killed after ~30s idle. WebSocket traffic counts as
  // activity, so a 20s ping keeps the socket (and the worker) alive.
  state.ping = setInterval(() => {
    if (!isConnected()) return;
    send({ type: "ping", session: state.session, t: Date.now() });
  }, 20000);
}

function stopKeepalive() {
  if (state.ping) {
    clearInterval(state.ping);
    state.ping = null;
  }
}

/**
 * Capture paths run on events that can wake a freshly restarted worker, where
 * the socket is not up yet. Reconnect aggressively and wait briefly instead of
 * letting the download slip back to the browser.
 */
async function ensureConnected(timeoutMs = 1500) {
  if (isConnected()) return true;
  state.portIndex = 0;
  state.reconnectDelay = RECONNECT_MIN;
  if (state.reconnectTimer) {
    clearTimeout(state.reconnectTimer);
    state.reconnectTimer = null;
  }
  connect();
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (isConnected()) return true;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  return isConnected();
}

const chunkAcks = new Map();
const extractionReplies = new Map();
const extractionCache = new Map();
const extractorCooldowns = new Map();
// Aynı video tek sefer analiz edilir: YouTube'da &t=, &pp=, embed/shorts varyantları aynı anahtara düşer.
const extractionKey = (tabId, pageUrl) => `${tabId}:${Lib.youtubeVideoUrl(pageUrl) || pageUrl}`;
async function extractorCandidates(tabId, pageUrl) {
  if (!state.settings.capture_enabled || !/^https?:/i.test(pageUrl || "")) return [];
  const host = new URL(pageUrl).hostname;
  if ((state.settings.excluded_hosts || []).some(h => host === h || host.endsWith(`.${h}`))) return [];
  if (!(await ensureConnected()) || !state.features.includes("ytdlp")) return [];
  if (!state.features.includes("ytdlp_context")) {
    extractionCache.set(extractionKey(tabId, pageUrl), { settled: true, items: [], error: { code: "update", message: t("errReopenForYtdlp"), retry_after: 0 } });
    return [];
  }
  const cooldown = extractorCooldowns.get(host);
  const key = extractionKey(tabId, pageUrl);
  if (cooldown && cooldown.until > Date.now()) {
    const current = extractionCache.get(key);
    if (!current) extractionCache.set(key, { at: Date.now(), items: [], error: cooldown.error, settled: true, promise: Promise.resolve([]) });
    return current?.items || [];
  }
  const cached = extractionCache.get(key);
  if (cached && (!cached.settled || cached.items?.length || Date.now() - cached.at < 300000)) return cached.promise;
  const promise = (async () => {
    const request = await buildRequest({ url: pageUrl, kind: "file", tabId, pageUrl });
    await attachExtractorContext(request, tabId);
    const id = crypto.randomUUID();
    const reply = await new Promise(resolve => {
      const timer = setTimeout(() => { extractionReplies.delete(id); resolve({ error: { code: "timeout", message: t("errAnalyzeTimeout"), retry_after: 60 } }); }, 65000);
      extractionReplies.set(id, { resolve, timer });
      if (!send({ type: "extract", id, request, session: state.session })) {
        clearTimeout(timer); extractionReplies.delete(id); resolve({ error: { code: "connection", message: t("errDisconnected"), retry_after: 0 } });
      }
    });
    const title = reply?.title;
    const height = Number(reply?.height) || null;
    const items = title ? [{ url: pageUrl, kind: "file", extractor: "ytdlp", height,
      label: height ? `yt-dlp · ${height}p · ${title}` : `yt-dlp · ${title}`,
      filename: Lib.sanitizeFilename(`${title}.mp4`), pageUrl }] : [];
    const entry = extractionCache.get(key); if (entry) { entry.items = items; entry.error = reply?.error || null; entry.settled = true; }
    if (reply?.error?.retry_after) {
      if (extractorCooldowns.size > 60) extractorCooldowns.delete(extractorCooldowns.keys().next().value);
      extractorCooldowns.set(host, { until: Date.now() + reply.error.retry_after * 1000, error: reply.error });
    }
    return items;
  })().catch(() => {
    const entry = extractionCache.get(key);
    if (entry) { entry.items = []; entry.settled = true; entry.error = { code: "context", message: t("errContext"), retry_after: 0 }; }
    return [];
  });
  if (extractionCache.size > 60) extractionCache.delete(extractionCache.keys().next().value);
  extractionCache.set(key, { at: Date.now(), promise });
  return promise;
}
async function combinedVideoCandidates(tabId, frameId, mediaUrl, pageUrl) {
  // The native options remain visible while the independent extractor works.
  void extractorCandidates(tabId, pageUrl);
  const cached = extractionCache.get(extractionKey(tabId, pageUrl));
  // YouTube linkleri yt-dlp adayıyla aynı video; listede iki kez görünmesin.
  return [...videoCandidates(tabId, frameId, mediaUrl, pageUrl).filter(c => !Lib.youtubeVideoUrl(c.url)), ...(cached?.items || [])];
}


function handleAppMessage(message) {
  switch (message.type) {
    case "extracted": {
      const pending = extractionReplies.get(message.id);
      if (pending) { clearTimeout(pending.timer); extractionReplies.delete(message.id); pending.resolve(message); }
      break;
    }
    case "refresh_context": {
      void (async () => {
        try {
          const page = await pageInfo(message.tab_id);
          const normalized = raw => { const url = new URL(raw); url.hash = ""; return url.href; };
          if (!page.url || (message.page_url && normalized(page.url) !== normalized(message.page_url))) throw new Error("session changed");
          const request = await buildRequest({ url: message.url, kind: "file", tabId: message.tab_id, pageUrl: page.url });
          await attachExtractorContext(request, message.tab_id);
          send({ type: "context", id: message.id, session: state.session, context: request.browser_context, error: null });
        } catch (_) {
          send({ type: "context", id: message.id, session: state.session, context: null, error: t("errSessionChanged") });
        }
      })();
      break;
    }
    case "bytes_ack": {
      const key = `${message.stream_id}:${message.index}`;
      const pending = chunkAcks.get(key);
      if (pending) { clearTimeout(pending.timer); chunkAcks.delete(key); pending.resolve(); }
      break;
    }
    case "hello_ok":
      state.session = message.session;
      state.appInfo = { app: message.app, version: message.version };
      // Son çalışan portu hatırla: her seferinde 8722'den başlayıp 8724/8725'e
      // kadar denemek konsolu gereksiz "CONNECTION_REFUSED" ile dolduruyordu.
      try {
        chrome.storage.local.set({ lastPort: state.port });
      } catch (_) {
        /* ignore */
      }
      state.features = message.features || [];
      if (message.settings) { Object.assign(state.settings, message.settings); void saveSettings(message.settings); }
      debug(`connected to ${message.app} ${message.version} on :${state.port}`);
      startKeepalive();
      setBadge();
      break;
    case "hello_err":
      console.warn("hazar: handshake rejected:", message.reason);
      break;
    case "grab_ack": {
      const pending = state.pending.get(message.id);
      if (pending) {
        clearTimeout(pending.timer);
        state.pending.delete(message.id);
        pending.resolve(true);
      }
      if (pending) state.stats.active += 1;
      updateRecent({
        id: message.id,
        state: message.state || "queued",
        written: 0,
        total: 0,
        at: Date.now(),
      });
      setBadge();
      break;
    }
    case "progress":
      updateRecent({
        id: message.id,
        state: message.phase,
        written: message.written,
        total: message.total,
        at: Date.now(),
      });
      break;
    case "finished":
      state.stats.active = Math.max(0, state.stats.active - 1);
      state.stats.done += 1;
      state.grabFallbacks.delete(message.id);
      updateRecent({
        id: message.id,
        state: "done",
        written: message.size,
        total: message.size,
        path: message.path,
        at: Date.now(),
      });
      setBadge();
      break;
    case "failed": {
      state.stats.active = Math.max(0, state.stats.active - 1);
      state.stats.failed += 1;
      updateRecent({ id: message.id, state: "failed", error: message.reason, at: Date.now() });
      setBadge();
      for (const [key, pending] of chunkAcks) {
        if (key.startsWith(`${message.id}:`)) {
          clearTimeout(pending.timer); chunkAcks.delete(key); pending.reject(new Error(message.reason));
        }
      }
      // App motoru, oynatıcı oturumuna bağlı segmentte 403 alırsa (Referer/çerez
      // yetmiyor, token o isteğe bağlı) AYNI segmentleri oynatıcı frame'inde indirip
      // byte olarak akıtıyoruz — "Sayfada indir" yolu, çalıştığı kanıtlı.
      const fallback = state.grabFallbacks.get(message.id);
      if (fallback) {
        state.grabFallbacks.delete(message.id);
        if (!fallback.tried && statusish(message.reason)) {
          fallback.tried = true;
          debug("app failed, oynatıcı oturumundan indirmeye geçiliyor", message.reason);
          tunnelSegments(fallback).catch((error) => debug("tunnel fallback failed", String(error)));
        }
      }
      break;
    }
    case "settings":
      if (message.settings) void saveSettings(message.settings);
      break;
    case "pong":
      break;
    case "error":
      console.warn("hazar: app error:", message.reason);
      break;
    default:
      console.debug("hazar: unhandled app message", message);
  }
}

async function sendGrab(request, timeout = ACK_TIMEOUT) {
  if (!(await ensureConnected())) {
    debug("grab dropped: not connected", request.url);
    return false;
  }
  const id = (crypto.randomUUID && crypto.randomUUID()) || `g${Date.now()}${Math.random()}`;
  const grab = { type: "grab", session: state.session, id, request };

  const acked = new Promise((resolve) => {
    const timer = setTimeout(() => {
      state.pending.delete(id);
      resolve(false);
    }, timeout);
    state.pending.set(id, { resolve, timer });
  });

  if (!send(grab)) {
    debug("grab send failed", request.url);
    return false;
  }
  debug("grab sent", id, request.kind, request.url);
  // HLS + segment listesi + tab varsa: app 403 alırsa aynı segmentleri oynatıcı
  // oturumunda indirip app'e byte olarak aktarabilmek için bilgiyi sakla.
  if (
    request &&
    request.kind === "hls" &&
    Array.isArray(request.segments) &&
    request.segments.length &&
    typeof request.tab_id === "number"
  ) {
    const candidate = findCandidate(request.tab_id, request.url)
      || [...(state.candidates.get(`${request.tab_id}`) || new Map()).values()]
        .find(c => c.frameUrl && c.frameUrl === request.frame_url);
    state.grabFallbacks.set(id, {
      tabId: request.tab_id,
      frameId: candidate && typeof candidate.frameId === "number" ? candidate.frameId : undefined,
      id,
      segments: request.segments,
      manifest: request.manifest || null,
      filename: request.filename || null,
      tried: false,
    });
  }
  const ok = await acked;
  debug(ok ? "grab acked" : "grab not acked", id);
  return ok;
}

// ---------------------------------------------------------------------------
// webRequest bookkeeping
// ---------------------------------------------------------------------------

function record(requestId, url) {
  let record = state.requests.get(requestId);
  if (!record) {
    record = { url, at: Date.now() };
    state.requests.set(requestId, record);
    if (state.requests.size > 4000) {
      const cutoff = Date.now() - 120000;
      for (const [key, value] of state.requests) {
        if (value.at < cutoff) state.requests.delete(key);
      }
    }
  }
  return record;
}

function headerValue(headers, name) {
  if (!headers) return null;
  const wanted = name.toLowerCase();
  for (const header of headers) {
    if (String(header.name || "").toLowerCase() === wanted) return header.value;
  }
  return null;
}

function initWebRequest() {
  const filter = { urls: ["<all_urls>"] };
  const extra = ["extraHeaders"];

  const add = (target, listener, extraInfoSpec) => {
    try {
      chrome.webRequest[target].addListener(listener, filter, extraInfoSpec);
    } catch (error) {
      // Firefox rejects "extraHeaders"; retry without it.
      const fallback = (extraInfoSpec || []).filter((spec) => spec !== "extraHeaders");
      try {
        chrome.webRequest[target].addListener(listener, filter, fallback);
      } catch (inner) {
        console.warn(`hazar: ${target} listener failed`, inner);
      }
    }
  };

  add("onBeforeRequest", (details) => {
    state.counters.beforeRequest += 1;
    const item = record(details.requestId, details.url);
    item.method = details.method;
    item.tabId = details.tabId;
    item.type = details.type;
    if (details.requestBody) item.hasBody = true;
  });

  add(
    "onBeforeSendHeaders",
    (details) => {
      state.counters.beforeSendHeaders += 1;
      const item = record(details.requestId, details.url);
      item.method = details.method;
      item.tabId = details.tabId;
      item.type = details.type;
      try {
        item.headers = Lib.forwardableHeaders(details.requestHeaders);
        item.extractorHeaders = (details.requestHeaders || []).map(header => [header.name, header.value || ""]);
      } catch (error) {
        state.counters.errors += 1;
        debug("forwardableHeaders failed", String(error));
      }
      const cookie = headerValue(details.requestHeaders, "cookie");
      if (cookie) item.cookie = cookie;
      const referer = headerValue(details.requestHeaders, "referer");
      if (referer) item.referer = referer;
      const agent = headerValue(details.requestHeaders, "user-agent");
      if (agent) item.userAgent = agent;
    },
    ["requestHeaders", ...extra],
  );

  debug("webRequest listeners installed");
  add(
    "onHeadersReceived",
    (details) => {
      state.counters.headersReceived += 1;
      const item = record(details.requestId, details.url);
      item.mime = headerValue(details.responseHeaders, "content-type") || item.mime;
      item.disposition =
        headerValue(details.responseHeaders, "content-disposition") || item.disposition;
      const length = headerValue(details.responseHeaders, "content-length");
      if (length) item.size = Number(length) || 0;
      item.status = details.statusCode;
      item.cloudflare = !!headerValue(details.responseHeaders, "cf-ray");
      item.tabId = typeof details.tabId === "number" ? details.tabId : item.tabId;
      item.pageUrl = details.initiator || details.originUrl || item.pageUrl;
      // The frame that made the request: a player CDN usually expects the
      // *player's* URL as Referer, not the top page.
      item.frameUrl = details.documentUrl || details.initiator || item.frameUrl;
      item.frameId = details.frameId;
      state.byUrl.set(details.url, { ...item, at: Date.now() });
      if (state.byUrl.size > 4000) pruneByUrl();

      let kind = "file";
      try {
        kind = Lib.pickKind(details.url, item.mime);
      } catch (error) {
        state.counters.errors += 1;
        debug("pickKind failed", String(error));
      }
      if (kind === "hls") {
        state.counters.manifests += 1;
        // `tabId < 0` = extension'in kendi fetch'i (örn. manifest probe) ya da
        // tab'a bağlı olmayan istek. Kendi probe'unu sniff'lemek hayalet aday
        // üretiyordu; yalnızca gerçek sekme trafiğini izle.
        if (state.settings.group_hls && details.tabId >= 0) {
          noteManifest(details.url, item);
        }
      }
      let isMedia = false;
      try {
        isMedia = Lib.isMedia(details.url, item.mime) || kind !== "file";
      } catch (error) {
        state.counters.errors += 1;
        debug("isMedia failed", String(error));
      }
      // Extension-less segments (MSE / token URLs) are only visible by MIME.
      const segmentMime = /^(video\/mp2t|video\/mp4|audio\/mp4)\b/i.test(item.mime || "");
      if (segmentMime && kind === "file" && details.tabId >= 0) {
        noteSegment(details.tabId, details.url, item.frameUrl || item.pageUrl);
      }
      const belongsToStream = kind === "file" && segmentMime && [...(state.candidates.get(`${details.tabId}`) || new Map()).values()]
        .some(c => c.isManifest && c.frameId === item.frameId);
      if (isMedia && details.tabId >= 0 && !belongsToStream) {
        addCandidate(details.tabId, {
          url: details.url,
          kind,
          mime: item.mime,
          size: item.size || null,
          pageUrl: item.pageUrl || null,
          filename: Lib.outputName({
            url: details.url,
            mime: item.mime,
            disposition: item.disposition,
          }),
          frameUrl: item.frameUrl || null,
          // frameId bilinmiyorsa alanı hiç koyma: bir page-hook adayının doğru
          // frameId'sini null ile ezmesin.
          ...(typeof item.frameId === "number" ? { frameId: item.frameId } : {}),
        });
      }
    },
    ["responseHeaders", ...extra],
  );

  add("onBeforeRedirect", (details) => {
    state.byUrl.delete(details.url);
  });
}

function pruneByUrl() {
  const cutoff = Date.now() - 120000;
  for (const [url, value] of state.byUrl) {
    if (value.at < cutoff) state.byUrl.delete(url);
  }
}

function metaFor(url) {
  return state.byUrl.get(url) || null;
}

// ---------------------------------------------------------------------------
// HLS / DASH sniffing
// ---------------------------------------------------------------------------

function bucketKey(url) {
  const groups = Lib.groupSegments([url]);
  return groups.length ? groups[0].key : url;
}

function noteSegment(tabId, url, pageUrl) {
  if (!Lib.isSegmentUrl(url)) return;
  const key = `${tabId}`;
  if (!state.segments.has(key)) state.segments.set(key, new Map());
  const buckets = state.segments.get(key);
  const bucket = bucketKey(url);
  if (!buckets.has(bucket)) buckets.set(bucket, { urls: new Set(), pageUrl: pageUrl || null });
  const entry = buckets.get(bucket);
  entry.urls.add(url);
  if (pageUrl) entry.pageUrl = entry.pageUrl || pageUrl;
}

async function noteManifest(url, item) {
  if (state.manifests.has(url)) return;
  const tabId = item.tabId ?? -1;
  // Tab'a bağlı olmayan istek (extension'ın kendi probe'u / browser) için aday
  // üretme: hayalet "-1" adayı ve yanlış "expired" bundan geliyordu.
  if (tabId < 0) return;
  state.manifests.add(url);
  addCandidate(tabId, {
    url,
    kind: Lib.pickKind(url, item.mime),
    mime: item.mime || null,
    size: null,
    pageUrl: item.pageUrl || null,
    filename: Lib.outputName({ url, mime: item.mime, disposition: item.disposition }),
    isManifest: true,
  });

  // Oynatıcının kendi yanıtından düz metin playlist gövdesi geldiyse yeniden
  // fetch etme: tek kullanımlık token'ı tüketir ve gereksiz 403 ile adayı
  // yanlışlıkla "expired" işaretler.
  const capture = () => {
    const candidate = findCandidate(tabId, url);
    const hasBody =
      candidate &&
      typeof candidate.manifest === "string" &&
      candidate.manifest.trimStart().startsWith("#EXTM3U");
    const hasSegments = candidate && Array.isArray(candidate.segments) && candidate.segments.length > 0;
    return { candidate, hasBody, hasSegments: hasBody || hasSegments };
  };
  if (capture().hasSegments) {
    debug("manifest probe skipped (page-hook body already captured)", url);
    return;
  }

  // Playlist'i oynatıcının kendi frame'inde çek: Referer/çerez/oturum oradan
  // gelir. Arka planda fetch etmek CDN'in 403'ünü adayın "süresi dolmuş"
  // işaretine çeviriyordu (yanlış sinyal) — artık yalnızca gövdeyi almayı dener.
  const fetched = await fetchInFrame(
    tabId,
    url,
    typeof item.frameId === "number" ? item.frameId : undefined,
  );
  let text = null;
  if (fetched && fetched.ok && fetched.base64) {
    try {
      text = atob(fetched.base64);
    } catch (_) {
      text = null;
    }
  }
  const status = fetched ? fetched.status : "?";

  if (text && text.trimStart().startsWith("#EXTM3U")) {
    const segments = Lib.segmentsFromPlaylist(text, url);
    // Buraya noteSegment KOYMA: playlist'ten çıkan göreli URL'ler token'sızdır;
    // sniff kovasına yazınca gerçek (token'lı) istekleri gölgelerdi.
    const { candidate, hasSegments } = capture();
    if (candidate && !hasSegments) {
      candidate.segments = segments;
      candidate.manifest = text;
      candidate.encrypted = false;
      candidate.expired = false;
    }
    return;
  }

  if (text) {
    // 200 ama playlist değil (HTML/şifreli gövde) → oynatıcı bunu JS'te çözüyor,
    // app bu bağlantıyı indiremez. Adayı işaretle, kullanıcı boşuna göndermesin.
    const { candidate: current, hasSegments: captured } = capture();
    if (current && !captured) {
      current.encrypted = true;
      current.mime = item.mime || current.mime;
    }
    debug("manifest probe body is not a playlist", url);
    return;
  }

  // Gövde alınamadı: bu "bağlantının süresi doldu" demek değil. Gerçek durum
  // indirme anında (oynatıcı frame'inde) ortaya çıkar.
  debug("manifest probe could not read a body", status, url);
}

/** Resolve the complete media playlist before either download path. */
async function segmentsForStream(tabId, url, frameId, provided) {
  // Sniffed requests are a playback window, not proof of a complete video.
  const resolved = await resolveSegments(tabId, url, frameId, 3);
  if (resolved.segments && resolved.segments.length) {
    resolved.segments.manifest = resolved.manifest;
    resolved.segments.manifestUrl = resolved.manifestUrl;
    resolved.segments.audio = resolved.audio || null;
    resolved.segments.capturePlan = resolved.capturePlan;
    return resolved.segments;
  }
  return { error: resolved.error || t("errNoPlaylist") };
}

/** Oynatıcının kendi manifest yanıtı: düz metinse segmentleri çıkar, değilse işaretle. */
function handleManifestBody(tabId, url, body, pageUrl, frameId, frameUrl) {
  if (!url || typeof body !== "string") return;
  const text = body.trimStart();
  if (/^(<\?xml[^>]*>\s*)?<MPD[\s>]/i.test(text)) {
    let candidate = findCandidate(tabId, url);
    if (!candidate) {
      addCandidate(tabId, { url, kind: "dash", mime: "application/dash+xml", pageUrl, frameId, frameUrl, filename: "stream.mp4", isManifest: true });
      candidate = findCandidate(tabId, url);
    }
    if (candidate) {
      candidate.manifest = body; candidate.encrypted = false;
      candidate.frameId = frameId; candidate.frameUrl = frameUrl;
    }
    return;
  }
  const plaintext = text.startsWith("#EXTM3U");
  let candidate = findCandidate(tabId, url) || findCandidateByUrl(url);
  if (!candidate) {
    candidate = { url, kind: Lib.pickKind(url, null), mime: null, pageUrl };
    addCandidate(tabId, candidate);
    candidate = findCandidate(tabId, url) || findCandidateByUrl(url);
  }
  if (!candidate) return;
  // Segmentleri oynatıcının kendi frame'inde indirmek için frameId şart; aksi
  // halde fetch yanlış frame'den gider ve CDN 403 döner.
  if (typeof frameId === "number" && frameId >= 0) candidate.frameId = frameId;
  // Frame URL'i de sakla: app'in göndereceği Referer bu olmalı.
  if (frameUrl) candidate.frameUrl = frameUrl;

  if (!plaintext) {
    candidate.encrypted = true;
    debug("manifest body is not a playlist (client-side encrypted)", url);
    return;
  }

  candidate.kind = "hls";
  candidate.isManifest = true;
  candidate.manifest = body;
  candidate.encrypted = false;
  candidate.expired = false;
  const segments = Lib.segmentsFromPlaylist(text, url);
  if (segments.length) {
    candidate.segments = segments;
    candidate.manifest = body;
    candidate.encrypted = false;
    candidate.expired = false;
    debug("manifest body captured:", segments.length, "segment(ler)", url);
  }
}

function addCandidate(tabId, candidate) {
  const key = `${tabId}`;
  if (!state.candidates.has(key)) state.candidates.set(key, new Map());
  const map = state.candidates.get(key);
  const existing = map.get(candidate.url);
  state.counters.candidates += existing ? 0 : 1;
  const merged = { ...existing, ...candidate, at: Date.now() };
  // Bilinmeyen frameId, bilinen bir frameId'yi ezmesin.
  if (
    (merged.frameId == null || merged.frameId < 0) &&
    existing &&
    typeof existing.frameId === "number" &&
    existing.frameId >= 0
  ) {
    merged.frameId = existing.frameId;
  }
  map.set(candidate.url, merged);
  if (map.size > 60) {
    const entries = [...map.entries()].sort((a, b) => (a[1].at || 0) - (b[1].at || 0));
    const oldest = entries.find(([, c]) => !c.isManifest) || entries[0];
    if (oldest) map.delete(oldest[0]);
  }
}

function findCandidate(tabId, url) {
  const map = state.candidates.get(`${tabId}`);
  return map ? map.get(url) : null;
}

function candidatesFor(tabId) {
  const map = state.candidates.get(`${tabId}`);
  const buckets = state.segments.get(`${tabId}`);
  const list = map ? [...map.values()] : [];

  for (const item of list) {
    item.streams = [];
    item.isMaster = typeof item.manifest === "string" && item.manifest.includes("#EXT-X-STREAM-INF");
  }

  if (buckets) {
    const groups = [...buckets.entries()].map(([bucket, entry]) => ({
      bucket,
      segments: [...entry.urls],
      pageUrl: entry.pageUrl,
    }));
    groups.sort((a, b) => b.segments.length - a.segments.length);
    const withoutManifest = groups.filter((group) => group.segments.length >= 3);
    if (withoutManifest.length && !list.some((item) => item.isManifest)) {
      list.push({
        url: withoutManifest[0].segments[0],
        kind: "hls",
        mime: null,
        pageUrl: withoutManifest[0].pageUrl,
        filename: "sniffed-stream.ts",
        isSniffed: true,
        segments: withoutManifest[0].segments,
      });
    }
    for (const item of list) {
      item.isMaster =
        typeof item.manifest === "string" && item.manifest.includes("#EXT-X-STREAM-INF");
    }
    const allSniffed = groups.flatMap((group) => group.segments);
    for (const item of list) {
      if (item.isManifest) continue;
      const group = groups.find((candidate) => candidate.segments[0] === item.url);
      if (group) item.streams = group.segments;
    }
    for (const item of list) {
      if (Array.isArray(item.segments) && item.segments.length && allSniffed.length) {
        item.segments = Lib.preferSniffedSegments(item.segments, allSniffed);
      }
    }
  }

  return list.sort((a, b) => (b.isManifest ? 1 : 0) - (a.isManifest ? 1 : 0));
}

/** Panel candidates are scoped to the requesting player frame. */
function videoCandidates(tabId, frameId, mediaUrl, frameUrl) {
  if (!state.settings.capture_enabled || (state.settings.excluded_hosts || []).some(host => {
    try { const current = new URL(frameUrl).hostname; return current === host || current.endsWith(`.${host}`); } catch (_) { return false; }
  })) return [];

  const list = candidatesFor(tabId).filter(c => c.frameId === frameId && !c.encrypted && !c.isSniffed);
  const direct = list.find(c => c.url === mediaUrl && !c.isSegment);
  if (direct && ["media", "file"].includes(direct.kind)) return [{ url: direct.url, kind: "file", label: "Video", filename: direct.filename }];
  const hidden = new Set();
  for (const c of list.filter(c => c.isMaster)) {
    for (const line of c.manifest.split(/\r?\n/)) {
      if (!line.startsWith("#EXT-X-MEDIA:") || !/TYPE=AUDIO(?:,|$)/.test(line)) continue;
      const match = /URI="([^"]+)"/.exec(line);
      try { if (match) hidden.add(new URL(match[1], c.url).href); } catch (_) {}
    }
  }
  const masters = list.filter(c => c.isMaster && !hidden.has(c.url));
  const options = [];
  for (const master of masters) {
    hidden.add(master.url);
    const variants = Lib.variantsFromPlaylist(master.manifest, master.url);
    for (const variant of variants) {
      hidden.add(variant.url);
      try {
        const audio = Lib.audioRenditionFromPlaylist(master.manifest, master.url, variant.url);
        if (audio) hidden.add(audio);
      } catch (_) {}
      options.push({ url: variant.url, kind: "hls",
        label: variant.height ? `${variant.height}p` : `${Math.round(variant.bandwidth / 1000)} kbps`,
        filename: master.filename });
    }
  }
  for (const c of list) {
    if (hidden.has(c.url) || c.isSegment || /^audio\//i.test(c.mime || "")) continue;
    if (list.some(item => item.isManifest) && c.kind === "file" && /^(video\/mp2t|video\/mp4|audio\/mp4)\b/i.test(c.mime || "")) continue;
    if (c.kind === "hls" && !c.isManifest) continue;
    if (!["hls", "dash", "media", "file"].includes(c.kind)) continue;
    options.push({ url: c.url, kind: c.kind === "media" ? "file" : c.kind,
      label: c.kind === "hls" ? "HLS" : c.kind === "dash" ? "DASH" : "Video", filename: c.filename });
  }
  return options.slice(0, 20);
}

// ---------------------------------------------------------------------------
// building and sending a grab
// ---------------------------------------------------------------------------

function getCookies(url) {
  return new Promise((resolve) => {
    try {
      chrome.cookies.getAll({ url }, (cookies) => resolve(cookies || []));
    } catch (_) {
      resolve([]);
    }
  });
}

async function extractorCookies(raw, tabId, frameId = 0) {
  const url = new URL(raw);
  const stores = await chrome.cookies.getAllCookieStores();
  const storeId = stores.find(store => store.tabIds.includes(tabId))?.id;
  const current = await chrome.cookies.getAll({ url: raw, ...(storeId ? { storeId } : {}) });
  const domains = new Set([url.hostname, ...current.map(cookie => cookie.domain.replace(/^\./, ""))]);
  const lists = await Promise.all([...domains].map(domain => chrome.cookies.getAll({ domain, ...(storeId ? { storeId } : {}) })));
  let partitioned = [];
  if (chrome.cookies.getPartitionKey) {
    try {
      const result = await chrome.cookies.getPartitionKey({ tabId, frameId });
      if (result?.partitionKey) partitioned = (await Promise.all([...domains].map(domain => chrome.cookies.getAll({ domain, partitionKey: result.partitionKey, ...(storeId ? { storeId } : {}) })))).flat();
    } catch (_) { /* Older browsers and unloaded frames have no partition API. */ }
  }
  const unique = new Map();
  for (const cookie of [...lists.flat(), ...partitioned]) {
    const domain = cookie.domain.replace(/^\./, "");
    if (url.hostname !== domain && !(cookie.domain.startsWith(".") && url.hostname.endsWith(`.${domain}`))) continue;
    // Only the selected frame partition is projected into this job’s private jar.
    unique.set(`${cookie.domain}:${cookie.path}:${cookie.name}`, { domain: cookie.hostOnly ? domain : `.${domain}`,
      path: cookie.path, name: cookie.name, value: cookie.value, secure: cookie.secure,
      http_only: cookie.httpOnly, expires: Math.floor(cookie.expirationDate || 0) });
  }
  return [...unique.values()];
}

async function extractorSessionCookies(pageUrl, tabId) {
  const urls = new Map([[pageUrl, 0]]);
  for (const candidate of state.candidates.get(tabId)?.values() || []) {
    if (/^https?:/i.test(candidate.frameUrl || "") && urls.size < 8) urls.set(candidate.frameUrl, candidate.frameId || 0);
  }
  const cookies = (await Promise.all([...urls].map(([url, frameId]) => extractorCookies(url, tabId, frameId)))).flat();
  return [...new Map(cookies.map(c => [`${c.domain}:${c.path}:${c.name}`, c])).values()];
}

function bypassProxy(raw, patterns) {
  const url = new URL(raw);
  if (!patterns.includes("<-loopback>") && ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname)) return true;
  return patterns.some(pattern => {
    pattern = String(pattern).trim();
    if (pattern === "<local>") return !url.hostname.includes(".") || url.hostname === "localhost";
    if (pattern === "<-loopback>") return false;
    if (pattern.includes("/" ) && !pattern.includes("://")) throw new Error("proxy CIDR bypass unsupported");
    if (pattern.startsWith(".")) pattern = `*${pattern}`;
    const rule = pattern.includes("://") ? pattern : `*://${pattern}`;
    const escaped = rule.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*");
    return new RegExp(`^${escaped}(?:/.*)?$`, "i").test(`${url.protocol}//${url.host}/`);
  });
}
async function extractorNetwork(raw, tabId) {
  const configured = String(state.settings.ytdlp_proxy || "").trim();
  const source_address = String(state.settings.ytdlp_source_address || "").trim() || null;
  if (configured) return { proxy: configured === "direct" ? "" : configured, source_address };
  if (!chrome.proxy?.settings?.get) return { proxy: null, source_address };
  try {
    const tab = await chrome.tabs.get(tabId);
    const result = await chrome.proxy.settings.get({ incognito: !!tab.incognito });
    const config = result.value;
    if (config?.mode === "direct") return { proxy: "", source_address };
    if (config?.mode === "fixed_servers") {
      const rules = config.rules || {};
      if (bypassProxy(raw, rules.bypassList || [])) return { proxy: "", source_address };
      const rule = rules.singleProxy || (new URL(raw).protocol === "https:" ? rules.proxyForHttps : rules.proxyForHttp) || rules.fallbackProxy;
      if (!rule) return { proxy: "", source_address };
      const scheme = rule.scheme === "socks5" ? "socks5h" : rule.scheme || "http";
      const host = rule.host.includes(":") ? `[${rule.host}]` : rule.host;
      return { proxy: `${scheme}://${host}:${rule.port || (scheme === "https" ? 443 : scheme.startsWith("socks") ? 1080 : 80)}`, source_address };
    }
    if (["pac_script", "auto_detect"].includes(config?.mode)) return { proxy: null, source_address, network_error: t("errPac") };
    return { proxy: null, source_address }; // system uses the downloader's system/environment proxy.
  } catch (_) { return { proxy: null, source_address, network_error: t("errProxyRead") }; }
}
async function attachExtractorContext(request, tabId) {
  const page = await pageInfo(tabId);
  const scopes = new Map();
  let userAgent = request.user_agent;
  const relevant = new Set([new URL(request.url).origin]);
  if (page.url) relevant.add(new URL(page.url).origin);
  for (const candidate of state.candidates.get(tabId)?.values() || []) {
    for (const raw of [candidate.url, candidate.frameUrl]) {
      if (/^https?:/i.test(raw || "")) relevant.add(new URL(raw).origin);
    }
  }
  for (const meta of [...state.byUrl.values()].sort((a, b) => (a.at || 0) - (b.at || 0))) {
    if (meta.tabId !== tabId || !/^https?:/i.test(meta.url || "")) continue;
    if (Date.now() - meta.at > 300000) continue;
    const origin = new URL(meta.url).origin;
    if (!relevant.has(origin)) continue;
    const headers = (meta.extractorHeaders || meta.headers || []).filter(([name]) => !["cookie", "host", "range", "referer", "user-agent", "content-length", "proxy-authorization", "connection", "accept-encoding", "accept", "content-type", "if-range", "if-none-match", "if-modified-since"].includes(name.toLowerCase()) && !name.toLowerCase().startsWith("sec-"));
    if (headers.length) {
      const previous = new Map(scopes.get(origin) || []);
      for (const [name, value] of headers) previous.set(name.toLowerCase(), value);
      scopes.set(origin, [...previous]);
    }
    if (origin === new URL(request.url).origin && meta.userAgent) userAgent = meta.userAgent;
  }
  const host = new URL(request.url).hostname;
  const meta = state.byUrl.get(request.url);
  request.browser_context = { cookies: await extractorSessionCookies(request.url, tabId),
    referer: request.referer || page.url, user_agent: userAgent,
    scoped_headers: (Lib.youtubeVideoUrl(request.url) ? [] : [...scopes].slice(-32)).map(([url, headers]) => ({ url, headers })),
    ...(await extractorNetwork(request.url, tabId)),
    impersonate: (state.settings.ytdlp_impersonate_hosts || []).some(h => host === h || host.endsWith(`.${h}`)) || !!(meta?.cloudflare && meta.status === 403) };
  request.browser_cookies = request.browser_context.cookies;
  request.cookie = null;
}

function pageInfo(tabId) {
  return new Promise((resolve) => {
    if (tabId == null || tabId < 0) return resolve({ url: null, title: null });
    try {
      chrome.tabs.get(tabId, (tab) => {
        if (chrome.runtime.lastError || !tab) return resolve({ url: null, title: null });
        resolve({ url: tab.url || null, title: tab.title || null });
      });
    } catch (_) {
      resolve({ url: null, title: null });
    }
  });
}

async function buildRequest({ url, kind, meta, item, tabId, pageUrl, pageTitle }) {
  // Aday, oynatıcı frame'inde gözlenmişse Referer oradan gelir. webRequest
  // meta'sı yoksa (aday yalnızca page-hook'tan geldiyse) adayın frameUrl'ine düş:
  // aksi halde app 'Referer'ı üst sayfa sanıp CDN'den 403 alıyordu.
  const frameCandidate = findCandidate(typeof tabId === "number" ? tabId : -1, url) || findCandidateByUrl(url);
  const cookies =
    (meta && meta.cookie) ||
    (frameCandidate && frameCandidate.cookie) ||
    Lib.cookieHeader(await getCookies(url));
  const headers = (meta && meta.headers ? meta.headers : []).filter(
    ([name]) => String(name).toLowerCase() !== "cookie",
  );
  const request = {
    url,
    kind: kind === "media" ? "file" : kind || Lib.pickKind(url, meta && meta.mime),
    filename: Lib.outputName({
      url,
      mime: meta && meta.mime,
      disposition: meta && meta.disposition,
      filename: (item && item.filename) || undefined,
      pageTitle,
    }),
    mime: (meta && meta.mime) || null,
    size: (meta && meta.size) || (item && (item.fileSize || item.totalBytes)) || null,
    method: (meta && meta.method) || "GET",
    referer:
      (meta && meta.frameUrl) ||
      (meta && meta.referer) ||
      (frameCandidate && frameCandidate.frameUrl) ||
      pageUrl ||
      (item && item.referrer) ||
      null,
    user_agent:
      (meta && meta.userAgent) ||
      (frameCandidate && frameCandidate.userAgent) ||
      navigator.userAgent,
    cookie: cookies || null,
    headers,
    page_url: pageUrl || (meta && meta.pageUrl) || null,
    frame_url:
      (meta && meta.frameUrl) ||
      (frameCandidate && frameCandidate.frameUrl) ||
      null,
    tab_id: typeof tabId === "number" ? tabId : (meta && meta.tabId) ?? null,
    save_dir: null,
    segments: null,
    manifest: null,
  };

  if (request.kind === "hls" || request.kind === "dash") {
    const candidate = frameCandidate;
    if (candidate) {
      request.segments = candidate.segments || null;
      request.manifest = candidate.manifest || null;
    }
  }
  return request;
}

function findCandidateByUrl(url) {
  for (const map of state.candidates.values()) {
    const found = map.get(url);
    if (found) return found;
  }
  return null;
}

// ---------------------------------------------------------------------------
// capture paths
// ---------------------------------------------------------------------------

function initDownloads() {
  if (!chrome.downloads) return;
  debug("downloads listeners installed");
  chrome.downloads.onCreated.addListener((item) => {
    debug("downloads.onCreated", item.url, `bytes=${item.totalBytes}`);
    maybeTakeOverDownload(item).catch((error) => {
      debug("takeover error", String(error));
      console.warn("hazar: download takeover failed", error);
    });
  });
  // Chromium-only: lets us learn the final file name early.
  if (chrome.downloads.onDeterminingFilename) {
    chrome.downloads.onDeterminingFilename.addListener(() => false);
  }
}

async function maybeTakeOverDownload(item) {
  const url = item.finalUrl || item.url;
  const meta = metaFor(url);
  if (meta && meta.method && meta.method !== "GET") {
    // POST/PUT bodies cannot be replayed by the engine yet — leave those alone.
    console.debug("hazar: skipping non-GET download", url);
    return;
  }

  const size = (meta && meta.size) || item.fileSize || item.totalBytes || 0;
  const decision = Lib.shouldCapture({
    url,
    size,
    mime: meta && meta.mime,
    settings: state.settings,
  });
  if (!decision.ok) {
    debug("takeover skipped", decision.reason, url);
    return;
  }
  if (!(await ensureConnected())) {
    debug("takeover skipped: app not connected", url);
    return;
  }
  if (Lib.youtubeVideoUrl(url) && !state.features.includes("youtube")) return;
  debug("takeover", url, `size=${size}`, `mime=${(meta && meta.mime) || "-"}`);

  const tabId = meta && typeof meta.tabId === "number" ? meta.tabId : null;
  const page = await pageInfo(tabId);
  const request = await buildRequest({
    url,
    meta,
    item,
    tabId,
    pageUrl: (item.referrer || (meta && meta.pageUrl)) ?? page.url,
    pageTitle: page.title,
  });

  // Keep the browser download recoverable until the app acknowledges ownership.
  await new Promise(resolve => chrome.downloads.pause(item.id, () => {
    void chrome.runtime.lastError; resolve();
  }));
  const acked = await sendGrab(request);
  if (acked) {
    chrome.downloads.cancel(item.id, () => {
      void chrome.runtime.lastError;
      chrome.downloads.erase({ id: item.id }, () => { void chrome.runtime.lastError; });
    });
  } else {
    chrome.downloads.resume(item.id, () => { void chrome.runtime.lastError; });
  }
}

/** Segmenti, manifesti gördüğümüz frame'de indirir (Referer/çerez oynatıcınınki). */
async function fetchInFrame(tabId, url, frameId, range = null) {
  const ask = (options) =>
    new Promise((resolve) => {
      try {
        chrome.tabs.sendMessage(tabId, { type: "fetch_bytes", url, range }, options, (response) =>
          resolve(response || { ok: false, error: "content script yok" }),
        );
      } catch (error) {
        resolve({ ok: false, error: String(error) });
      }
    });

  if (typeof frameId === "number") {
    const inFrame = await ask({ frameId });
    if (inFrame.ok) return inFrame;
  }
  const anyFrame = await ask();
  if (anyFrame.ok) return anyFrame;

  // Son çare: background'dan indir (bazı CDN'ler bunu kabul eder).
  try {
    const response = await fetch(url, { credentials: "include" });
    if (!response.ok) return { ok: false, status: response.status };
    const buffer = await response.arrayBuffer();
    const bytes = new Uint8Array(buffer);
    let binary = "";
    for (let index = 0; index < bytes.length; index += 0x8000) {
      binary += String.fromCharCode.apply(null, bytes.subarray(index, index + 0x8000));
    }
    return { ok: true, base64: btoa(binary) };
  } catch (error) {
    return { ok: false, error: String(error) };
  }
}

/**
 * Playlist'i (gerekiyorsa master → varyant zincirini) çözer ve segment listesini verir.
 * Segment istekleri oynatıcının frame'inden yapıldığı için token/çerez/Referer tutar.
 */
async function resolveSegments(tabId, url, frameId, depth, includeAudio = true) {
  // Önce oynatıcının kendi request'inden yakalanmış gövdeyi kullan. Master URL
  // tek kullanımlıksa onu yeniden fetch etmek 403/404 üretir; eski akışın hatası buydu.
  const captured = findCandidate(tabId, url);
  let text = captured && typeof captured.manifest === "string" ? captured.manifest : null;

  if (!text || !text.trimStart().startsWith("#EXTM3U")) {
    const fetched = await fetchInFrame(tabId, url, frameId);
    if (!fetched || !fetched.ok) {
      return {
        error: `yakalanmış playlist gövdesi yok; yeniden fetch status ${fetched ? fetched.status : "?"}`,
      };
    }
    try {
      text = atob(fetched.base64 || "");
    } catch (_) {
      return { error: t("errPlaylistBody") };
    }
  }

  if (!text.trimStart().startsWith("#EXTM3U")) {
    return { error: t("errPlaylistText") };
  }
  const variant = Lib.bestVariantFromPlaylist(text, url);
  if (variant) {
    if (depth <= 0) return { error: t("errPlaylistChain") };
    const video = await resolveSegments(tabId, variant, frameId, depth - 1, false);
    if (video.error || !includeAudio) return video;
    const audioUrl = Lib.audioRenditionFromPlaylist(text, url, variant);
    if (audioUrl) {
      const audio = await resolveSegments(tabId, audioUrl, frameId, depth - 1, false);
      if (audio.error) return { error: `Audio playlist: ${audio.error}` };
      video.audio = audio;
    }
    return video;
  }
  if (!text.includes("#EXT-X-ENDLIST")) return { error: t("errNotVod") };
  let audio = null;
  if (includeAudio) {
    const masters = candidatesFor(tabId).filter(c => c.manifest && c.manifest.includes("#EXT-X-STREAM-INF"));
    for (const master of masters) {
      const audioUrl = Lib.audioRenditionFromPlaylist(master.manifest, master.url, url);
      if (!audioUrl) continue;
      audio = await resolveSegments(tabId, audioUrl, frameId, depth - 1, false);
      if (audio.error) return { error: `Audio playlist: ${audio.error}` };
      break;
    }
  }
  const parsed = Lib.segmentsFromPlaylist(text, url);
  const capturePlan = Lib.capturePlanFromPlaylist(text, url);
  // Playlist satırları göreliyse token düşer; oynatıcının gerçekten istediği
  // (sniff edilmiş) segment URL'lerini tercih et.
  const buckets = state.segments.get(`${tabId}`);
  const sniffed = buckets ? [...buckets.values()].flatMap((entry) => [...entry.urls]) : [];
  const resolvedUrls = parsed.length && sniffed.length ? Lib.preferSniffedSegments(parsed, sniffed) : parsed;
  let mediaIndex = 0;
  for (const part of capturePlan) { if (!part.init) part.url = resolvedUrls[mediaIndex++]; }
  return {
    manifest: text,
    manifestUrl: url,
    audio,
    capturePlan,
    segments: resolvedUrls,
  };
}

/** App hatası durum gibi mi (403/401/timeout) — fallback kararı için. */
const statusish = Lib.statusish;

/**
 * Segmentleri oynatıcı frame'inde indirip app'e byte olarak akıtır. Token/session
 * bağlı CDN'lerde app motoru 403 alırken bu yol çalışır: istek oynatıcının kendi
 * oturumundan (Referer/çerez/UA) çıkar.
 */
async function tunnelSegments({ tabId, frameId, segments, filename, id, manifest, audio = segments.audio }) {
  if (!(await ensureConnected())) return { ok: false, error: t("errNotConnected") };
  if (audio && !state.features.includes("hls_audio_bytes")) return { ok: false, error: t("errUpdateAudio") };
  let capturePlan = segments.capturePlan || (manifest ? Lib.capturePlanFromPlaylist(manifest, segments.manifestUrl || segments[0]) : segments.map(url => ({ url })));
  const audioStart = audio ? capturePlan.length : null;
  if (audio) capturePlan = [...capturePlan, ...audio.capturePlan];
  const keys = new Map();
  if (!state.features.includes("bytes_ack")) return { ok: false, error: t("errUpdateApp") };
  const streamId = id || `cap-${Date.now().toString(36)}`;
  const name = filename || "stream.ts";
  if (!segments.length) return { ok: false, error: t("errNoSegments") };
  const fail = error => {
    updateRecent({ id: streamId, state: "failed", error, at: Date.now() });
    send({ type: "capture_failed", session: state.session, id: streamId, reason: error });
    return { ok: false, error, streamId };
  };
  const total = capturePlan.length;
  updateRecent({ id: streamId, state: "capturing", written: 0, total, at: Date.now() });
  for (let index = 0; index < total; index += 1) {
    const segment = capturePlan[index];
    const result = await fetchInFrame(tabId, segment.url, frameId, segment.range);
    if (result?.ok && segment.key) {
      try {
        let key = keys.get(segment.key.url);
        if (!key) {
          const response = await fetchInFrame(tabId, segment.key.url, frameId);
          if (!response?.ok) throw new Error(t("errHlsKey"));
          const raw = Uint8Array.from(atob(response.base64), c => c.charCodeAt(0));
          if (raw.length !== 16) throw new Error(t("errHlsKeyLength"));
          key = await crypto.subtle.importKey("raw", raw, "AES-CBC", false, ["decrypt"]);
          keys.set(segment.key.url, key);
        }
        const encrypted = Uint8Array.from(atob(result.base64), c => c.charCodeAt(0));
        const iv = Uint8Array.from(segment.key.iv.match(/../g), hex => parseInt(hex, 16));
        const plain = new Uint8Array(await crypto.subtle.decrypt({ name: "AES-CBC", iv }, key, encrypted));
        let binary = "";
        for (let offset = 0; offset < plain.length; offset += 0x8000) binary += String.fromCharCode(...plain.subarray(offset, offset + 0x8000));
        result.base64 = btoa(binary);
      } catch (error) { result.ok = false; result.error = String(error); }
    }
    if (!result || !result.ok) {
      const reason =
        result && result.status
          ? `status ${result.status}`
          : (result && result.error) || "bilinmeyen hata";
      const error = `segment ${index + 1}/${total}: ${reason}`;
      return fail(error);
    }
    const prefix = atob(result.base64 || "").slice(0, 512).trimStart();
    if (/^(#EXTM3U|<\?xml|<MPD[\s>]|<!doctype|<html[\s>])/i.test(prefix)) {
      const error = `segment ${index + 1}/${total}: video yerine playlist/HTML geldi; kaydedilmedi`;
      return fail(error);
    }
    const key = `${streamId}:${index}`;
    const ack = new Promise((resolve, reject) => {
      const timer = setTimeout(() => { chunkAcks.delete(key); reject(new Error("app chunk ACK timeout")); }, 30000);
      chunkAcks.set(key, { resolve, reject, timer });
    });
    const sent = send({
      type: "bytes",
      session: state.session,
      stream_id: streamId,
      index,
      total,
      audio_start: audioStart,
      url: segment.url,
      filename: name,
      data_b64: result.base64,
    });
    if (!sent) {
      const pending = chunkAcks.get(key); clearTimeout(pending.timer); chunkAcks.delete(key);
      return fail(t("errAppNotConnected"));
    }
    try { await ack; } catch (error) {
      return fail(String(error));
    }
    updateRecent({ id: streamId, state: "capturing", written: index + 1, total, at: Date.now() });
  }
  return { ok: true, segments: total, streamId };
}

async function submitGrab(message, tabId, pageUrl, frameId, frameUrl) {
  const youtube = Lib.youtubeVideoUrl(message.url);
  if (message.extractor === "ytdlp" || youtube) {
    if (!(await ensureConnected())) return { ok: false, error: t("errNotConnected"), connected: false };
    if (!state.features.includes("ytdlp")) return { ok: false, error: t("errUpdateYtdlp") };
  }
  if (youtube) {
    message = { ...message, url: youtube, kind: "file" };
  }
  const request = await buildRequest({
    url: message.url,
    kind: message.kind,
    meta: metaFor(message.url) || message.meta || null,
    item: null,
    tabId,
    pageUrl: message.pageUrl || pageUrl,
    pageTitle: message.pageTitle || null,
  });
  if (message.extractor === "ytdlp" || youtube) {
    request.extractor = "ytdlp";
    request.kind = "file";
    await attachExtractorContext(request, tabId);
  }
  if (frameUrl && !request.frame_url) {
    request.frame_url = frameUrl;
    request.referer = frameUrl;
  }
  if (message.filename) request.filename = Lib.sanitizeFilename(message.filename);
  if (message.segments && message.segments.length) request.segments = message.segments;
  // HLS'te app motoruna manifest URL'i vermek yanlıştı: app onu oynatıcı
  // oturumu olmadan tekrar çeker ve tek kullanımlık token yüzünden 403 alır.
  // Gerçek (token'lı) segment listesini biz çözüp veriyoruz.
  if (request.kind === "hls") {
    const candidate = findCandidate(tabId, message.url) || findCandidateByUrl(message.url);
    const resolved = await segmentsForStream(
      tabId,
      message.url,
      candidate && typeof candidate.frameId === "number" ? candidate.frameId : frameId ?? undefined,
      message.segments,
    );
    if (!Array.isArray(resolved) || resolved.length === 0) {
      const reason =
        (resolved && resolved.error) ||
        t("errSegmentsHint");
      return { ok: false, error: reason, connected: isConnected() };
    }
    request.segments = resolved;
    request.manifest = resolved.manifest || null;
    request.url = resolved.manifestUrl || request.url;
    if (resolved.audio) {
      // Separate tracks require browser session fetches followed by app mux.
      if (!(await ensureConnected()) || !state.features.includes("hls_audio_bytes")) {
        return { ok: false, error: t("errUpdateAudio") };
      }
      const id = crypto.randomUUID();
      // Acknowledge the popup immediately; the app reports final mux status.
      void tunnelSegments({ tabId, id,
        frameId: candidate?.frameId ?? frameId ?? undefined,
        segments: resolved, manifest: resolved.manifest,
        filename: request.filename }).then(result => {
          if (!result.ok) updateRecent({ id, state: "failed", error: result.error, at: Date.now() });
        }).catch(error => {
          updateRecent({ id, state: "failed", error: String(error), at: Date.now() });
        });
      return { ok: true, connected: true };
    }
  }
  return { ok: await sendGrab(request), connected: isConnected() };
}

function initContextMenus() {
  if (!chrome.contextMenus) return;
  const build = () => chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({ id: "hazar-link", title: t("menuLink"), contexts: ["link", "video", "audio", "image"] });
    chrome.contextMenus.create({ id: "hazar-media", title: t("menuMedia"), contexts: ["page"] });
    chrome.contextMenus.create({ id: "hazar-links", title: t("menuLinks"), contexts: ["page"] });
    chrome.contextMenus.create({ id: "hazar-selection", title: t("menuSelection"), contexts: ["selection"] });
  });
  chrome.runtime.onInstalled.addListener(() => { void HazarI18n.ready.then(build); });
  HazarI18n.onChange(build);

  chrome.contextMenus.onClicked.addListener(async (info, tab) => {
    if (!tab?.id) return;
    try {
      const frameId = Number.isInteger(info.frameId) ? info.frameId : 0;
      const frameUrl = info.frameUrl || tab.url;
      if (["hazar-links", "hazar-selection"].includes(info.menuItemId)) {
        const result = await new Promise(resolve => chrome.tabs.sendMessage(tab.id,
          { type: "collect_download_links", selected: info.menuItemId === "hazar-selection" }, { frameId }, reply => {
            void chrome.runtime.lastError; resolve(reply);
          }));
        for (const link of result?.links || []) {
          const response = await submitGrab({ url: link.url, pageTitle: link.label }, tab.id, tab.url, frameId, frameUrl);
          if (!response.ok) break;
        }
        return;
      }
      const candidates = videoCandidates(tab.id, frameId, info.srcUrl, frameUrl);
      const candidate = (info.menuItemId === "hazar-media" || (info.mediaType === "video" && info.srcUrl?.startsWith("blob:"))) ? candidates[0] : null;
      const url = candidate?.url || info.linkUrl || info.srcUrl;
      if (!url || !/^https?:/i.test(url)) return;
      await submitGrab({ url, kind: candidate?.kind, pageTitle: tab.title }, tab.id, tab.url, frameId, frameUrl);
    } catch (error) { debug("context menu download failed", String(error)); }
  });
}

function initContentMessages() {
  chrome.runtime.onMessage.addListener((message, sender, respond) => {
    const tabId = sender && sender.tab ? sender.tab.id : (Number.isInteger(message && message.tabId) ? message.tabId : null);
    const pageUrl = sender && sender.tab ? sender.tab.url : null;
    const frameId = sender && typeof sender.frameId === "number" ? sender.frameId : null;
    // Frame'in kendi URL'i: page-hook'tan gelen adaylarda doğru Referer bu.
    const frameUrl = (sender && sender.url) || null;

    switch (message && message.type) {
      case "video_candidates":
        combinedVideoCandidates(tabId, frameId, message.mediaUrl, message.pageUrl || frameUrl).then(candidates => respond({ ok: true, candidates }));
        return true;
      case "page-hook":
        if (message.payload && message.payload.kind === "manifest-body") {
          handleManifestBody(
            tabId,
            message.payload.url,
            message.payload.body,
            pageUrl,
            frameId,
            frameUrl,
          );
          respond({ ok: true });
          return true;
        }
        if (message.payload && message.payload.kind === "segment") {
          noteSegment(tabId, message.payload.url, pageUrl);
        } else if (message.payload && message.payload.kind === "manifest") {
          addCandidate(tabId, {
            url: message.payload.url,
            kind: Lib.pickKind(message.payload.url, message.payload.mime),
            mime: message.payload.mime || null,
            pageUrl,
            frameId,
            frameUrl,
            filename: Lib.outputName({ url: message.payload.url, mime: message.payload.mime }),
            isManifest: true,
          });
        }
        respond({ ok: true });
        return true;
      case "media-candidate":
        addCandidate(tabId, { ...message.payload, pageUrl, frameId, frameUrl });
        respond({ ok: true });
        return true;
      case "grab":
        submitGrab(message, tabId, pageUrl, frameId, frameUrl)
          .then(respond).catch(error => respond({ ok: false, error: String(error) }));
        return true;
      case "candidates": {
        const id = message.tabId ?? tabId;
        pageInfo(id).then(page => {
          void extractorCandidates(id, page.url);
          const extra = extractionCache.get(extractionKey(id, page.url))?.items || [];
          respond({ ok: true, candidates: [...candidatesFor(id).filter(c => !Lib.youtubeVideoUrl(c.url)), ...extra],
            extractor: extractionCache.get(extractionKey(id, page.url)) ? { pending: !extractionCache.get(extractionKey(id, page.url)).settled, error: extractionCache.get(extractionKey(id, page.url)).error || null } : null });
        }).catch(() => respond({ ok: true, candidates: candidatesFor(id) }));
        return true;
      }
      case "retry_extractor": {
        pageInfo(message.tabId).then(page => {
          const host = page.url ? new URL(page.url).hostname : "";
          const cooldown = extractorCooldowns.get(host);
          if (cooldown && cooldown.until > Date.now() && cooldown.error.code === "rate_limit") { respond({ ok: false, error: cooldown.error.message }); return; }
          extractionCache.delete(extractionKey(message.tabId, page.url)); extractorCooldowns.delete(host);
          void extractorCandidates(message.tabId, page.url);
          respond({ ok: true });
        }).catch(() => respond({ ok: false, error: t("errTabNotFound") }));
        return true;
      }
      case "status":
        respond({
          ok: true,
          connected: isConnected(),
          port: state.port,
          app: state.appInfo,
          features: state.features,
          settings: state.settings,
          stats: state.stats,
        });
        return true;
      case "settings":
        saveSettings(message.patch || {}).then(() => respond({ ok: true, settings: state.settings }));
        return true;
      case "reconnect":
        state.portIndex = 0;
        state.reconnectDelay = RECONNECT_MIN;
        try {
          if (state.socket) state.socket.close();
        } catch (_) {
          /* ignore */
        }
        state.socket = null;
        state.session = null;
        connect();
        respond({ ok: true });
        return true;
      case "save_stream": {
        // Segmentleri oynatıcının kendi frame'inde indir, baytları app'e aktar.
        const streamId = `cap-${Date.now().toString(36)}`;
        const filename = message.filename || "stream.ts";
        const frameId = typeof message.frameId === "number" ? message.frameId : undefined;
        updateRecent({ id: streamId, state: "resolving", written: 0, total: 0, at: Date.now() });
        (async () => {
          // Popup listesi → aday segmentleri → sniff edilmiş → çözümleme.
          // Manifesti tekrar fetch etmek tek kullanımlık token'ı yakıp 403
          // üretiyordu (kullanıcının gördüğü hata).
          const segments = await segmentsForStream(
            message.tabId,
            message.url,
            frameId,
            message.segments,
          );
          if (!Array.isArray(segments) || segments.length === 0) {
            const reason = (segments && segments.error) || t("errSegmentNotFound");
            updateRecent({ id: streamId, state: "failed", error: reason, at: Date.now() });
            respond({ ok: false, error: reason });
            return;
          }
          updateRecent({ id: streamId, state: "capturing", written: 0, total: segments.length, at: Date.now() });
          const result = await tunnelSegments({
            tabId: message.tabId,
            frameId,
            segments,
            filename,
            id: streamId,
            manifest: segments.manifest || (findCandidate(message.tabId, message.url) || {}).manifest || null,
          });
          if (!result.ok) {
            // Popup'a bağlamlı hata ver: bağlamsız "status 403" yanlış mesaja
            // (ve anlamsız tekrar-oynat döngüsüne) yol açıyordu.
            respond({ ok: false, error: result.error });
            return;
          }
          respond({ ok: true, segments: result.segments });
        })();
        return true;
      }
      case "recapture": {
        // Redirect the next navigation to our recapture page so the app can
        // refresh an expired session, then fall back to the page.
        const ruleId = Math.floor(Math.random() * 100000) + 1000;
        const escaped = message.url.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
        chrome.declarativeNetRequest
          .updateSessionRules({
            addRules: [
              {
                id: ruleId,
                priority: 1,
                action: {
                  type: "redirect",
                  redirect: {
                    regexSubstitution: `${chrome.runtime.getURL("recapture.html")}?rule=${ruleId}#\\0`,
                  },
                },
                condition: { regexFilter: `^${escaped}$`, resourceTypes: ["main_frame"] },
              },
            ],
          })
          .then(() => respond({ ok: true, ruleId }))
          .catch((error) => respond({ ok: false, error: String(error) }));
        return true;
      }
      default:
        return false;
    }
  });
}

chrome.runtime.onMessage.addListener((message, _sender, _respond) => {
  if (message && message.type === "clear-recapture-rule") {
    chrome.declarativeNetRequest
      .updateSessionRules({ removeRuleIds: [message.ruleId] })
      .catch(() => {});
  }
  return false;
});

// ---------------------------------------------------------------------------
// lifecycle
// ---------------------------------------------------------------------------

async function boot() {
  await loadSettings();
  connect();
  setBadge();
}

// MV3 rule: listeners must be registered during the initial *synchronous* run of
// the service worker, otherwise Chrome will not start the worker for those events.
// Registering them behind `await loadSettings()` silently dropped every cold-start
// download and sniff (found by the js-only browser self-test).
initWebRequest();
initDownloads();
initContextMenus();
initContentMessages();

chrome.runtime.onStartup.addListener(boot);
chrome.runtime.onInstalled.addListener(() => {
  boot();
});
chrome.alarms.create("hazar-keepalive", { periodInMinutes: 1 });
chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name !== "hazar-keepalive") return;
  if (!isConnected()) {
    connect();
  } else {
    send({ type: "ping", session: state.session, t: Date.now() });
  }
  if (state.stats.active === 0 && state.stats.failed > 0) state.stats.failed = 0;
  setBadge();
});

boot();

chrome.cookies.onChanged.addListener(() => {
  for (const [key, entry] of extractionCache) { if (entry.settled && entry.error?.code === "login") extractionCache.delete(key); }
});
chrome.tabs.onRemoved.addListener(tabId => {
  for (const key of extractionCache.keys()) if (key.startsWith(`${tabId}:`)) extractionCache.delete(key);
});

chrome.tabs.onUpdated.addListener((tabId, change) => {
  if (change.status === "loading" || change.url) {
    for (const key of extractionCache.keys()) if (key.startsWith(`${tabId}:`)) extractionCache.delete(key);
  }
});
