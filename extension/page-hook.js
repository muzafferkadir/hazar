/**
 * Hazar Integration — page-context hook (injected by content.js).
 *
 * Observes the requests the page itself makes (`XMLHttpRequest`, `fetch`) and
 * reports HLS/DASH manifests and media segments back to the content script.
 *
 * Unlike IDM's equivalent, nothing is rewritten: we do not inject request
 * identifiers into page requests, we only look at URLs and Content-Type.
 * Every hook is wrapped so a failure can never break the host page.
 */
(() => {
  const SOURCE = "hazar-page-hook";
  const MANIFEST = /\.(m3u8|m3u|mpd)(\?|#|$)/i;
  const SEGMENT = /\.(ts|m4s|m4v|m4a|aac|cmfv|cmfa)(\?|#|$)/i;
  const MANIFEST_MIME = /(mpegurl|dash\+xml|octet-stream-m3u8)/i;

  function post(kind, url, mime, body) {
    try {
      window.postMessage(
        { source: SOURCE, kind, url: String(url), mime: mime || null, body: body || null },
        "*",
      );
    } catch (_) {
      /* ignore */
    }
  }

  /** Manifest yanıtının gövdesini de bildir: token tek kullanımlıksa app tekrar
   *  çekemiyor, ama gövde elimizde olursa segmentleri ondan çıkarabiliriz. */
  function reportManifestBody(url, mime, text) {
    if (typeof text !== "string" || text.length === 0 || text.length > 512 * 1024) return;
    post("manifest-body", url, mime, text);
  }

  function classify(url, mime) {
    const text = String(mime || "");
    if (MANIFEST.test(url) || MANIFEST_MIME.test(text)) return "manifest";
    if (SEGMENT.test(url)) return "segment";
    // Token/segment URLs often carry no extension at all; the MIME is the only
    // signal (dplayer82-style players do this).
    if (/^(video\/mp2t|video\/mp4|audio\/mp4)\b/i.test(text)) return "segment";
    return null;
  }

  function reverseUrl(url) {
    try {
      return new URL(String(url), location.href).toString();
    } catch (_) {
      return null;
    }
  }

  // --- XMLHttpRequest -------------------------------------------------------
  try {
    const proto = window.XMLHttpRequest && window.XMLHttpRequest.prototype;
    if (proto && proto.open && proto.send) {
      const originalOpen = proto.open;
      const originalSend = proto.send;

      proto.open = function (method, url, ...rest) {
        try {
          this.__hazarUrl = reverseUrl(url);
        } catch (_) {
          /* ignore */
        }
        return originalOpen.call(this, method, url, ...rest);
      };

      proto.send = function (...args) {
        try {
          const url = this.__hazarUrl;
          if (url) {
            this.addEventListener("load", () => {
              try {
                const mime = this.getResponseHeader && this.getResponseHeader("content-type");
                const kind = classify(url, mime);
                if (kind) post(kind, url, mime);
                if (kind === "manifest" && typeof this.responseText === "string") {
                  reportManifestBody(url, mime, this.responseText);
                }
              } catch (_) {
                /* ignore */
              }
            });
          }
        } catch (_) {
          /* ignore */
        }
        return originalSend.apply(this, args);
      };
    }
  } catch (_) {
    /* ignore */
  }

  // --- fetch ---------------------------------------------------------------
  try {
    const originalFetch = window.fetch;
    if (typeof originalFetch === "function") {
      window.fetch = function (input, init) {
        const promise = originalFetch.apply(this, arguments);
        try {
          const raw = typeof input === "string" ? input : input && input.url;
          const url = raw ? reverseUrl(raw) : null;
          if (url && promise && typeof promise.then === "function") {
            promise
              .then((response) => {
                try {
                  const mime = response && response.headers && response.headers.get("content-type");
                  const kind = classify(url, mime);
                  if (kind) post(kind, url, mime);
                  if (kind === "manifest" && response && typeof response.clone === "function") {
                    response
                      .clone()
                      .text()
                      .then((text) => reportManifestBody(url, mime, text))
                      .catch(() => {});
                  }
                } catch (_) {
                  /* ignore */
                }
              })
              .catch(() => {});
          }
        } catch (_) {
          /* ignore */
        }
        return promise;
      };
    }
  } catch (_) {
    /* ignore */
  }

  // --- MediaSource ---------------------------------------------------------
  // Tells us whether the page is playing an HLS-ish (video/mp2t, mpegurl) or
  // DASH-ish (mp4, webm) stream; the actual segments show up via XHR/fetch.
  try {
    const MediaSourceClass = window.MediaSource;
    const proto = MediaSourceClass && MediaSourceClass.prototype;
    if (proto && proto.addSourceBuffer) {
      const original = proto.addSourceBuffer;
      proto.addSourceBuffer = function (mime) {
        try {
          post("mse", "", mime);
        } catch (_) {
          /* ignore */
        }
        return original.call(this, mime);
      };
    }
  } catch (_) {
    /* ignore */
  }
})();
