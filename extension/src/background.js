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
    importScripts("lib.js");
  } catch (error) {
    console.error("hazar: could not load lib.js", error);
  }
}
const Lib = globalThis.HazarLib;

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
  stats: { active: 0, done: 0, failed: 0 },
  counters: { beforeRequest: 0, beforeSendHeaders: 0, headersReceived: 0, manifests: 0, candidates: 0, errors: 0 },
  ping: null,
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
  log: state.log.slice(-25),
});

// ---------------------------------------------------------------------------
// settings / storage
// ---------------------------------------------------------------------------

function loadSettings() {
  return new Promise((resolve) => {
    chrome.storage.local.get(DEFAULT_SETTINGS, (stored) => {
      state.settings = { ...DEFAULT_SETTINGS, ...(stored || {}) };
      resolve(state.settings);
    });
  });
}

function saveSettings(patch) {
  state.settings = { ...state.settings, ...patch };
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
    chrome.action.setBadgeBackgroundColor({ color: state.stats.failed > 0 ? "#ff453a" : "#6366f1" });
    chrome.action.setTitle({
      title: state.port
        ? `Hazar — app listening on 127.0.0.1:${state.port}`
        : "Hazar — app is not running",
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
      const grouped = buckets
        ? [...buckets.values()]
            .map((entry) => [...entry.urls])
            .sort((a, b) => b.length - a.length)[0]
        : null;
      out.push({
        url: candidate.url,
        kind: candidate.kind || "file",
        mime: candidate.mime || null,
        size: candidate.size || null,
        isManifest: !!candidate.isManifest,
        pageUrl: candidate.pageUrl || null,
        frameUrl: candidate.frameUrl || null,
        encrypted: !!candidate.encrypted,
        segments: candidate.segments || grouped || null,
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
  const ports = state.settings.ports || DEFAULT_SETTINGS.ports;
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

function handleAppMessage(message) {
  switch (message.type) {
    case "hello_ok":
      state.session = message.session;
      state.appInfo = { app: message.app, version: message.version };
      state.features = message.features || [];
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
      state.stats.active += 1;
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
    case "failed":
      state.stats.active = Math.max(0, state.stats.active - 1);
      state.stats.failed += 1;
      updateRecent({ id: message.id, state: "failed", error: message.reason, at: Date.now() });
      setBadge();
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
      item.pageUrl = details.initiator || details.originUrl || item.pageUrl;
      // The frame that made the request: a player CDN usually expects the
      // *player's* URL as Referer, not the top page.
      item.frameUrl = details.documentUrl || details.initiator || item.frameUrl;
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
        if (state.settings.group_hls) {
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
      if (segmentMime && kind === "file") {
        noteSegment(details.tabId, details.url, item.frameUrl || item.pageUrl);
      }
      if (isMedia) {
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
  state.manifests.add(url);
  const tabId = item.tabId ?? -1;
  addCandidate(tabId, {
    url,
    kind: Lib.pickKind(url, item.mime),
    mime: item.mime || null,
    size: null,
    pageUrl: item.pageUrl || null,
    filename: Lib.outputName({ url, mime: item.mime, disposition: item.disposition }),
    isManifest: true,
  });

  // Pull the playlist so the app can download without re-fetching cookies.
  try {
    const response = await fetch(url, { credentials: "include" });
    if (!response.ok) return;
    const text = await response.text();
    if (!text.startsWith("#EXTM3U")) {
      // Playlist düz metin değil (HTML/şifreli gövde) → oynatıcı bunu JS'te çözüyor,
      // app bu bağlantıyı indiremez. Adayı işaretle, kullanıcı boşuna göndermesin.
      const candidate = findCandidate(tabId, url);
      if (candidate) {
        candidate.encrypted = true;
        candidate.mime = item.mime || candidate.mime;
      }
      debug("manifest is not plaintext (client-side encrypted?)", url);
      return;
    }
    const segments = text
      .split("\n")
      .map((line) => line.trim())
      .filter((line) => line && !line.startsWith("#"))
      .map((line) => {
        try {
          return new URL(line, url).toString();
        } catch (_) {
          return null;
        }
      })
      .filter(Boolean);
    for (const segment of segments) noteSegment(tabId, segment, item.pageUrl);
    const candidate = findCandidate(tabId, url);
    if (candidate) {
      candidate.segments = segments;
      candidate.manifest = text;
    }
  } catch (error) {
    console.debug("hazar: playlist fetch failed", error);
  }
}

function addCandidate(tabId, candidate) {
  const key = `${tabId}`;
  if (!state.candidates.has(key)) state.candidates.set(key, new Map());
  const map = state.candidates.get(key);
  const existing = map.get(candidate.url);
  state.counters.candidates += existing ? 0 : 1;
  map.set(candidate.url, { ...existing, ...candidate, at: Date.now() });
  if (map.size > 60) {
    const oldest = [...map.entries()].sort((a, b) => (a[1].at || 0) - (b[1].at || 0))[0];
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
      if (item.isManifest) continue;
      const group = groups.find((candidate) => candidate.segments[0] === item.url);
      if (group) item.streams = group.segments;
    }
  }

  return list.sort((a, b) => (b.isManifest ? 1 : 0) - (a.isManifest ? 1 : 0));
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
  const cookies = meta && meta.cookie ? meta.cookie : Lib.cookieHeader(await getCookies(url));
  const headers = (meta && meta.headers ? meta.headers : []).filter(
    ([name]) => String(name).toLowerCase() !== "cookie",
  );
  const request = {
    url,
    kind: kind || Lib.pickKind(url, meta && meta.mime),
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
    referer: (meta && meta.frameUrl) || (meta && meta.referer) || pageUrl || (item && item.referrer) || null,
    user_agent: (meta && meta.userAgent) || navigator.userAgent,
    cookie: cookies || null,
    headers,
    page_url: pageUrl || (meta && meta.pageUrl) || null,
    frame_url: (meta && meta.frameUrl) || (candidateForFrame && candidateForFrame.frameUrl) || null,
    tab_id: typeof tabId === "number" ? tabId : (meta && meta.tabId) ?? null,
    save_dir: null,
    segments: null,
    manifest: null,
  };

  const candidateForFrame = findCandidateByUrl(url);
  if (request.kind === "hls") {
    const candidate = findCandidate(request.tab_id ?? -1, url) || findCandidateByUrl(url);
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

  chrome.downloads.cancel(item.id, () => {
    try {
      chrome.downloads.erase({ id: item.id });
    } catch (_) {
      /* ignore */
    }
  });

  const acked = await sendGrab(request);
  if (!acked) {
    console.warn("hazar: app did not acknowledge — resuming the browser download");
    try {
      chrome.downloads.resume(item.id);
    } catch (_) {
      /* ignore */
    }
  }
}

function initContextMenus() {
  if (!chrome.contextMenus) return;
  chrome.runtime.onInstalled.addListener(() => {
    chrome.contextMenus.removeAll(() => {
      chrome.contextMenus.create({
        id: "hazar-link",
        title: "Download with Hazar",
        contexts: ["link", "video", "audio", "image"],
      });
      chrome.contextMenus.create({
        id: "hazar-media",
        title: "Send this stream to Hazar",
        contexts: ["page"],
      });
    });
  });

  chrome.contextMenus.onClicked.addListener(async (info, tab) => {
    const url = info.linkUrl || info.srcUrl || (info.pageUrl && info.pageUrl);
    if (!url) return;
    const meta = metaFor(url);
    const request = await buildRequest({
      url,
      meta,
      item: null,
      tabId: tab && tab.id,
      pageUrl: info.pageUrl || (tab && tab.url) || null,
      pageTitle: tab && tab.title,
    });
    if (!(await sendGrab(request))) {
      console.warn("hazar: app is not running");
    }
  });
}

function initContentMessages() {
  chrome.runtime.onMessage.addListener((message, sender, respond) => {
    const tabId = sender && sender.tab ? sender.tab.id : null;
    const pageUrl = sender && sender.tab ? sender.tab.url : null;

    switch (message && message.type) {
      case "page-hook":
        if (message.payload && message.payload.kind === "segment") {
          noteSegment(tabId, message.payload.url, pageUrl);
        } else if (message.payload && message.payload.kind === "manifest") {
          addCandidate(tabId, {
            url: message.payload.url,
            kind: Lib.pickKind(message.payload.url, message.payload.mime),
            mime: message.payload.mime || null,
            pageUrl,
            filename: Lib.outputName({ url: message.payload.url, mime: message.payload.mime }),
            isManifest: true,
          });
        }
        respond({ ok: true });
        return true;
      case "media-candidate":
        addCandidate(tabId, { ...message.payload, pageUrl });
        respond({ ok: true });
        return true;
      case "grab":
        buildRequest({
          url: message.url,
          kind: message.kind,
          meta: metaFor(message.url) || message.meta || null,
          item: null,
          tabId,
          pageUrl: message.pageUrl || pageUrl,
          pageTitle: message.pageTitle || null,
        })
          .then((request) => {
            if (message.segments && message.segments.length) request.segments = message.segments;
            return sendGrab(request);
          })
          .then((ok) => respond({ ok, connected: isConnected() }))
          .catch((error) => respond({ ok: false, error: String(error) }));
        return true;
      case "candidates":
        respond({ ok: true, candidates: candidatesFor(message.tabId ?? tabId) });
        return true;
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
