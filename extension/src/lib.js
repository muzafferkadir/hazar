/**
 * Hazar extension — pure helpers.
 *
 * Loaded three ways:
 *  - Chrome MV3 service worker: `importScripts("lib.js")` from background.js
 *  - Firefox MV3 event page: listed before background.js in manifest
 *  - Node (tests): `require("./lib.js")`
 *
 * No `chrome.*` usage here on purpose: everything in this file is testable.
 */
(function (root, factory) {
  const api = factory();
  root.HazarLib = api;
  if (typeof module !== "undefined" && module.exports) module.exports = api;
})(typeof globalThis !== "undefined" ? globalThis : this, function () {
  const MIME_KIND = [
    [/^application\/vnd\.apple\.mpegurl/i, "hls"],
    [/^application\/x-mpegurl/i, "hls"],
    [/^application\/octet-stream-m3u8/i, "hls"],
    [/^audio\/(x-)?mpegurl/i, "hls"],
    [/^video\/(x-)?mpegurl/i, "hls"],
    [/^application\/dash\+xml/i, "dash"],
    [/^video\/vnd\.mpeg\.dash\.mpd/i, "dash"],
  ];

  const MEDIA_MIME = /^(video|audio)\//i;

  const SEGMENT_EXT = /\.(ts|m4s|m4v|m4a|aac|mp4|cmfv|cmfa)$/i;

  function pathOf(url) {
    try {
      const parsed = new URL(url);
      return parsed.pathname;
    } catch (_) {
      return String(url || "").split(/[?#]/)[0];
    }
  }

  function isManifestUrl(url) {
    const path = pathOf(url).toLowerCase();
    return path.endsWith(".m3u8") || path.endsWith(".m3u") || path.endsWith(".mpd");
  }

  function isSegmentUrl(url) {
    return SEGMENT_EXT.test(pathOf(url));
  }

  function mediaType(mime) {
    return String(mime || "")
      .split(";")[0]
      .trim()
      .toLowerCase();
  }

  /** Decide how the app should treat this URL/response. */
  function pickKind(url, mime) {
    const type = mediaType(mime);
    for (const [pattern, kind] of MIME_KIND) {
      if (pattern.test(type)) return kind;
    }
    const path = pathOf(url).toLowerCase();
    if (path.endsWith(".m3u8") || path.endsWith(".m3u")) return "hls";
    if (path.endsWith(".mpd")) return "dash";
    return "file";
  }

  function isMedia(url, mime) {
    return MEDIA_MIME.test(mediaType(mime)) || isManifestUrl(url);
  }

  function hostOf(url) {
    try {
      return new URL(url).hostname.toLowerCase();
    } catch (_) {
      return "";
    }
  }

  function hostMatches(host, pattern) {
    const clean = String(pattern || "").trim().toLowerCase();
    if (!clean) return false;
    if (clean.startsWith("*.")) {
      const suffix = clean.slice(1);
      return host === clean.slice(2) || host.endsWith(suffix);
    }
    return host === clean || host.endsWith("." + clean);
  }

  /**
   * Should the extension take this download away from the browser?
   * Returns {ok: boolean, reason?: string}.
   */
  function shouldCapture({ url, size, mime, settings }) {
    settings = settings || {};
    const raw = String(url || "");
    if (!/^https?:/i.test(raw)) return { ok: false, reason: "scheme" };
    if (settings.capture_enabled === false) return { ok: false, reason: "capture disabled" };

    const minSize = Number(settings.min_size_bytes || 0);
    const knownSize = Number(size || 0);
    if (minSize > 0 && knownSize > 0 && knownSize < minSize) {
      return { ok: false, reason: "below min size" };
    }

    const host = hostOf(raw);
    for (const pattern of settings.excluded_hosts || []) {
      if (hostMatches(host, pattern)) return { ok: false, reason: "excluded host" };
    }
    for (const pattern of settings.deny_patterns || []) {
      try {
        if (new RegExp(pattern, "i").test(raw)) return { ok: false, reason: "deny pattern" };
      } catch (_) {
        /* ignore bad user regex */
      }
    }
    const kind = pickKind(raw, mime);
    if (kind !== "file" && settings.capture_manifests === false) {
      return { ok: false, reason: "manifest capture disabled" };
    }
    return { ok: true };
  }

  function filenameFromUrl(url) {
    const path = pathOf(url);
    const last = path.split("/").filter(Boolean).pop() || "download";
    try {
      return decodeURIComponent(last);
    } catch (_) {
      return last;
    }
  }

  function filenameFromDisposition(value) {
    const text = String(value || "");
    const match =
      /filename\*=UTF-8''([^;]+)/i.exec(text) || /filename="?([^";]+)"?/i.exec(text);
    if (!match) return null;
    try {
      return decodeURIComponent(match[1].trim());
    } catch (_) {
      return match[1].trim();
    }
  }

  function sanitizeFilename(name) {
    const cleaned = String(name || "")
      .replace(/[\\/:*?"<>|\u0000-\u001f]/g, "_")
      .trim()
      .replace(/^\.+|\.+$/g, "");
    return cleaned || "download";
  }

  /** Guess the output name for a job. */
  function outputName({ url, mime, disposition, filename, pageTitle }) {
    const fromHeader = filenameFromDisposition(disposition);
    const base = filename || fromHeader || filenameFromUrl(url);
    if (base) {
      const kind = pickKind(url, mime);
      if (kind === "hls" && /\.m3u8?$/i.test(base)) return sanitizeFilename(base.replace(/\.m3u8?$/i, ".ts"));
      if (base.includes(".")) return sanitizeFilename(base);
    }
    const kind = pickKind(url, mime);
    if (kind === "hls") return sanitizeFilename((pageTitle || "stream") + ".ts");
    if (kind === "dash") return sanitizeFilename((pageTitle || "stream") + ".mp4");
    return sanitizeFilename((pageTitle ? pageTitle + "-" : "") + (base || "download"));
  }

  function youtubeVideoUrl(raw) {
    try {
      const url = new URL(raw), parts = url.pathname.split('/').filter(Boolean);
      if (!['http:', 'https:'].includes(url.protocol)) return null;
      let id = null;
      if (url.hostname === 'youtu.be') id = parts[0];
      else if (['youtube.com', 'www.youtube.com', 'm.youtube.com', 'music.youtube.com', 'youtube-nocookie.com', 'www.youtube-nocookie.com'].includes(url.hostname)) {
        id = url.pathname === '/watch' ? url.searchParams.get('v') : ['embed', 'shorts', 'live'].includes(parts[0]) ? parts[1] : null;
      }
      return /^[a-zA-Z0-9_-]{11}$/.test(id || '') ? `https://www.youtube.com/watch?v=${id}` : null;
    } catch (_) { return null; }
  }

  function cookieHeader(cookies) {
    return (cookies || [])
      .filter((cookie) => cookie && cookie.name)
      .map((cookie) => `${cookie.name}=${cookie.value}`)
      .join("; ");
  }

  /**
   * Group sniffed segment URLs into per-stream buckets.
   * Segments of one stream normally share a directory and extension; sorting by
   * the trailing number keeps them in playback order.
   */
  function groupSegments(urls) {
    const groups = new Map();
    for (const url of urls || []) {
      if (!isSegmentUrl(url)) continue;
      let key;
      try {
        const parsed = new URL(url);
        const dir = parsed.pathname.split("/").slice(0, -1).join("/");
        key = `${parsed.host}${dir}`;
      } catch (_) {
        key = "raw";
      }
      const extension = (pathOf(url).match(SEGMENT_EXT) || [""])[0].toLowerCase();
      const bucket = `${key}|${extension}`;
      if (!groups.has(bucket)) groups.set(bucket, []);
      groups.get(bucket).push(url);
    }

    const out = [];
    for (const [key, list] of groups) {
      const unique = Array.from(new Set(list));
      unique.sort((a, b) => segmentIndex(a) - segmentIndex(b));
      out.push({ key, segments: unique, extension: key.split("|")[1] || "" });
    }
    out.sort((a, b) => b.segments.length - a.segments.length);
    return out;
  }

  /**
   * Bir m3u8 gövdesinden segment URL'lerini çıkarır (yorumlar/etiketler hariç).
   * Göreli URL'ler `baseUrl`e göre çözülür.
   */
  function segmentsFromPlaylist(text, baseUrl) {
    const source = String(text || "");
    if (!source.trimStart().startsWith("#EXTM3U") || /#EXT-X-STREAM-INF:/i.test(source)) return [];
    const out = [];
    for (const raw of source.split(String.fromCharCode(10))) {
      const line = raw.trim();
      if (!line || line.startsWith("#")) continue;
      try {
        out.push(new URL(line, baseUrl).toString());
      } catch (_) {
        /* bozuk satırı atla */
      }
    }
    return out;
  }

  /**
   * Master playlist ise en yüksek BANDWIDTH'li varyantın URL'ini döndürür.
   * (Gerçek bölümlerde içerik önce varyant playlist'te listelenir.)
   */
  function variantsFromPlaylist(text, baseUrl) {
    const variants = [];
    let pending = null;
    for (const raw of String(text || "").split(/\r?\n/)) {
      const line = raw.trim();
      if (line.startsWith("#EXT-X-STREAM-INF:")) {
        const bandwidth = /(?:^|,)BANDWIDTH=(\d+)/.exec(line.split(":").slice(1).join(":"));
        const resolution = /RESOLUTION=(\d+)x(\d+)/.exec(line);
        pending = { bandwidth: bandwidth ? Number(bandwidth[1]) : 0,
          height: resolution ? Number(resolution[2]) : null };
      } else if (pending && line && !line.startsWith("#")) {
        try { variants.push({ ...pending, url: new URL(line, baseUrl).href }); } catch (_) {}
        pending = null;
      }
    }
    return variants.sort((a, b) => b.bandwidth - a.bandwidth);
  }

  function bestVariantFromPlaylist(text, baseUrl) {
    return variantsFromPlaylist(text, baseUrl)[0]?.url || null;
  }

  function playlistAttributes(line) {
    return Object.fromEntries([...line.matchAll(/([A-Z0-9-]+)=("[^"]*"|[^,]*)/g)]
      .map(m => [m[1], m[2].replace(/^"|"$/g, "")]));
  }

  // Preserve initialization, ranges and AES-128 while planning browser capture.
  function capturePlanFromPlaylist(text, baseUrl) {
    const plan = [];
    let key = null, sequence = 0n, pendingRange = null, previous = null, previousMap = null, mapId = null;
    const ivFor = (explicit, seq) => {
      const value = explicit ? explicit.replace(/^0x/i, "") : seq.toString(16);
      if (!/^[0-9a-f]+$/i.test(value) || value.length > 32) throw new Error("HLS IV geçersiz");
      return value.padStart(32, "0");
    };
    const rangeFor = (value, url, prior) => {
      if (!value) return null;
      const match = /^(\d+)(?:@(\d+))?$/.exec(value);
      if (!match) throw new Error("HLS BYTERANGE geçersiz");
      const start = match[2] != null ? Number(match[2]) : prior?.url === url ? prior.end + 1 : NaN;
      const length = Number(match[1]), end = start + length - 1;
      if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start < 0 || length <= 0) throw new Error("HLS BYTERANGE offset bulunamadı");
      return { start, end };
    };
    for (const raw of String(text).split(/\r?\n/)) {
      const line = raw.trim();
      if (line.startsWith("#EXT-X-MEDIA-SEQUENCE:")) sequence = BigInt(line.split(":")[1]);
      else if (line.startsWith("#EXT-X-KEY:")) {
        const a = playlistAttributes(line);
        if (a.METHOD === "NONE") key = null;
        else if (a.METHOD === "AES-128" && (!a.KEYFORMAT || a.KEYFORMAT === "identity") && a.URI) {
          key = { url: new URL(a.URI, baseUrl).href, iv: a.IV || null };
        } else throw new Error("Bu HLS şifreleme yöntemi desteklenmiyor");
      } else if (line.startsWith("#EXT-X-BYTERANGE:")) pendingRange = line.split(":")[1];
      else if (line.startsWith("#EXT-X-MAP:")) {
        const a = playlistAttributes(line);
        if (!a.URI) throw new Error("HLS init segment URI bulunamadı");
        const url = new URL(a.URI, baseUrl).href;
        const range = rangeFor(a.BYTERANGE, url, previousMap);
        if (key && !key.iv) throw new Error("AES-128 init segment IV gerekli");
        const map = { url, range, init: true, key: key ? { url: key.url, iv: ivFor(key.iv, sequence) } : null };
        const id = JSON.stringify(map);
        if (mapId !== id) { plan.push(map); mapId = id; }
        previousMap = range ? { url, end: range.end } : null;
      } else if (line && !line.startsWith("#")) {
        const url = new URL(line, baseUrl).href;
        const range = rangeFor(pendingRange, url, previous);
        plan.push({ url, range, key: key ? { url: key.url, iv: ivFor(key.iv, sequence) } : null });
        previous = range ? { url, end: range.end } : null;
        pendingRange = null; sequence += 1n;
      }
    }
    return plan;
  }

  // Match the selected variant's AUDIO group, including quoted attribute values.
  function audioRenditionFromPlaylist(text, baseUrl, variantUrl) {
    const attributes = playlistAttributes;
    const lines = String(text || "").split(/\r?\n/).map(l => l.trim());
    let group = null;
    let pending = null;
    for (const line of lines) {
      if (line.startsWith("#EXT-X-STREAM-INF:")) pending = attributes(line);
      else if (pending && line && !line.startsWith("#")) {
        if (new URL(line, baseUrl).href === variantUrl) group = pending.AUDIO;
        pending = null;
      }
    }
    if (!group) return null;
    const choices = lines.filter(l => l.startsWith("#EXT-X-MEDIA:"))
      .map(attributes).filter(a => a.TYPE === "AUDIO" && a["GROUP-ID"] === group);
    if (!choices.length) throw new Error("Master playlist audio grubu bulunamadı");
    const chosen = choices.find(a => a.DEFAULT === "YES")
      || choices.find(a => a.AUTOSELECT === "YES") || choices[0];
    // No URI means the audio is embedded in the video rendition.
    return chosen.URI ? new URL(chosen.URI, baseUrl).href : null;
  }

  /**
   * Oynatıcının gerçekten istediği (ve 200 aldığı) segment URL'lerini tercih et.
   *
   * Playlist satırları göreliyse (`seg0.ts`) manifest'in `?token` sorgusu düşer;
   * oynatıcı ise segmente token'ı ekleyerek ister. Aynı path için sniff edilmiş
   * URL varsa onu kullan; henüz çekilmemiş kardeş segmentler için aynı dizinden
   * gözlenen sorguyu uygula — yoksa indirme 403 yer.
   */
  function preferSniffedSegments(segments, sniffed) {
    const list = sniffed || [];
    const byPath = new Map();
    const queryByDir = new Map();
    for (const url of list) {
      const path = pathOf(url);
      if (path && !byPath.has(path)) byPath.set(path, url);
      try {
        const parsed = new URL(url);
        const dir = `${parsed.host}|${parsed.pathname.split("/").slice(0, -1).join("/")}`;
        if (parsed.search && !queryByDir.has(dir)) queryByDir.set(dir, parsed.search);
      } catch (_) {
        /* ignore */
      }
    }
    return (segments || []).map((url) => {
      try {
        const parsed = new URL(url);
        // Query can identify a different playlist/resource on the same PHP endpoint.
        if (parsed.search) return url;
        const match = byPath.get(pathOf(url));
        if (match) return match;
        const dir = `${parsed.host}|${parsed.pathname.split("/").slice(0, -1).join("/")}`;
        const search = queryByDir.get(dir);
        if (!search) return url;
        parsed.search = search;
        return parsed.toString();
      } catch (_) {
        return url;
      }
    });
  }

  /**
   * App hatası "durum kaynaklı" mı (403/401/timeout)? Bu durumda segmentleri
   * oynatıcı oturumunda indiren byte-tunnel fallback'ine geçilir.
   */
  function statusish(reason) {
    return /40[13]|401|403|status|unexpected status|timed? ?out/i.test(String(reason || ""));
  }

  /** Hata metnindeki URL'nin kısa etiketi: host + path (query/token atılır).
   *  403'ün hangi segmentten geldiğini token sızdırmadan gösterir. */
  function urlLabelOf(message) {
    const match = String(message || "").match(/https?:\/\/[^\s"']+/i);
    if (!match) return null;
    try {
      const parsed = new URL(match[0]);
      const path = parsed.pathname && parsed.pathname !== "/" ? parsed.pathname : "";
      return `${parsed.host}${path}`;
    } catch (_) {
      return null;
    }
  }

  /**
   * Ham motor hatalarını kullanıcıya anlaşılır cümleye çevirir (app + popup ortak).
   * Yanlış "tekrar oynat" döngüsü yaratan mesajlar düzeltildi: segment 403'ü
   * manifest/oturum 403'ünden ayırır ve hatanın host'unu gösterir.
   */
  function humanizeError(message) {
    const text = String(message || "");
    const host = urlLabelOf(text);
    const where = host ? ` (${host})` : "";
    if (/yakalanmış playlist gövdesi yok/i.test(text)) {
      return 'oynatıcının playlist yanıtı yakalanmadı — videoyu oynat, popup\'ı yenile, sonra "Sayfada indir"';
    }
    if (/^segment \d+\/\d+:.*403/.test(text)) {
      return `CDN segment isteğini reddetti (403)${where} — segment URL'i oynatıcı oturumuna bağlı. Videoyu oynat, segmentler akarken popup'tan "Sayfada indir" kullan`;
    }
    if (/403/.test(text)) {
      return `CDN isteği 403 döndü${where} — bu URL oynatıcı oturumuna bağlı. Videoyu oynat, segmentler akarken popup'tan "Sayfada indir" kullan`;
    }
    if (/404/.test(text) && /(\.m3u8|\.mpd|l\.php)/.test(text)) {
      return "bağlantının süresi dolmuş (oynatıcı tek kullanımlık token) — videoyu oynatıp tekrar gönder";
    }
    if (/not an HLS playlist/.test(text)) {
      return "stream tarayıcıda şifreli çözülüyor — indirilemiyor";
    }
    if (/SAMPLE-AES|widevine|playready/i.test(text)) return "DRM korumalı — indirilemiyor";
    if (/timed out|timeout/i.test(text)) return `zaman aşımı: ${text}`;
    return text;
  }

  /** Trailing number of a segment URL; used for ordering. */
  function segmentIndex(url) {
    const path = pathOf(url);
    const matches = path.match(/(\d+)(?!.*\d)/);
    if (matches) return Number(matches[1]);
    return 0;
  }

  /** Pick the most promising sniffed stream for a tab. */
  function bestStream(segmentUrls, { minSegments = 3 } = {}) {
    const groups = groupSegments(segmentUrls);
    const candidate = groups.find((group) => group.segments.length >= minSegments);
    if (!candidate) return null;
    return candidate;
  }

  /**
   * Request headers worth forwarding to the app (skip hop-by-hop and the
   * headers the engine manages itself).
   */
  const DROP_HEADERS = new Set([
    "host",
    "connection",
    "content-length",
    "accept-encoding",
    "range",
    "if-range",
    "if-none-match",
    "if-modified-since",
    "upgrade-insecure-requests",
    "sec-fetch-dest",
    "sec-fetch-mode",
    "sec-fetch-site",
    "sec-fetch-user",
    "sec-ch-ua",
    "sec-ch-ua-mobile",
    "sec-ch-ua-platform",
    "accept-language",
  ]);

  function forwardableHeaders(headers) {
    const out = [];
    for (const header of headers || []) {
      const name = String(header.name || "").toLowerCase();
      if (!name || DROP_HEADERS.has(name)) continue;
      if (name.startsWith(":")) continue;
      out.push([header.name, String(header.value || "")]);
    }
    return out;
  }

  return {
    pickKind,
    isManifestUrl,
    isSegmentUrl,
    isMedia,
    mediaType,
    shouldCapture,
    filenameFromUrl,
    filenameFromDisposition,
    sanitizeFilename,
    outputName,
    cookieHeader,
    youtubeVideoUrl,
    groupSegments,
    segmentsFromPlaylist,
    bestVariantFromPlaylist,
    variantsFromPlaylist,
    audioRenditionFromPlaylist,
    capturePlanFromPlaylist,
    preferSniffedSegments,
    humanizeError,
    statusish,
    urlLabelOf,
    segmentIndex,
    bestStream,
    forwardableHeaders,
    hostMatches,
  };
});
