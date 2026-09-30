#!/usr/bin/env python3
"""
Hazar capture-flow check with Patchright (throwaway profile, local fixture).

Mimics the dizipal/dplayer82 shape:
  page -> iframe player -> fetch(token master.m3u8) -> fetch(token variant) -> fetch first segment

Gates (like a real CDN):
  - manifest + segment routes require `?t=<token>`
  - manifest + segment routes require a Referer header

The extension's *background* probe re-fetches the manifest WITHOUT a Referer,
so the CDN rejects it. Before the fix that 403 flipped the candidate to
"baglanti suresi dolmus" and the parsed (token-less) segment URLs failed on the
first segment.

Scope: manifest detection + the FIRST segment only. Nothing is downloaded in
full and nothing is handed to the Hazar app. No secrets/tokens are printed
(query values are redacted).
"""
import json
import os
import re
import socket
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOKEN = "TESTTOKEN"
SEG_BODY = b"\x47" + bytes(188 * 4 - 1)  # tiny fake TS payload, 4 packets

EXT = os.path.expanduser("~/Dev/re/hazar/extension")


def find_chrome():
    base = os.path.expanduser("~/Library/Caches/ms-playwright")
    candidates = []
    if os.path.isdir(base):
        for name in sorted(os.listdir(base)):
            cand = os.path.join(
                base, name, "chrome-mac-arm64",
                "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            )
            if os.path.exists(cand):
                candidates.append(cand)
    env = os.environ.get("HAZAR_CHROME")
    if env and os.path.exists(env):
        return env
    if not candidates:
        raise SystemExit("no Chrome for Testing found (set HAZAR_CHROME)")
    return candidates[-1]


CHROME = find_chrome()

HITS = []          # (path, has_query, has_referer, status)
HITS_LOCK = threading.Lock()

MASTER = """#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360
360/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720
720/index.m3u8
"""

VARIANT = """#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:6
#EXTINF:6.0,
seg0.ts
#EXTINF:6.0,
seg1.ts
#EXT-X-ENDLIST
"""

PAGE = """<!doctype html><html><body>
<iframe src="/player.html" allowfullscreen></iframe>
</body></html>"""

PLAYER = """<!doctype html><html><body>
<video controls></video>
<script src="/player.js"></script>
</body></html>"""

# Builds the token URL at runtime, pulls the master, resolves the variant
# RELATIVE TO THE MANIFEST (like a real player), then fetches the first segment.
PLAYER_JS = """(async function () {
  try {
    var t = '%s';
    var master = '/hls/master.m3u8?t=' + t;
    var r = await fetch(master);
    var text = await r.text();
    var lines = text.split(String.fromCharCode(10));
    var variant = null;
    for (var i = 0; i < lines.length; i++) {
      var l = lines[i].trim();
      if (l && l.charAt(0) !== '#') { variant = new URL(l, new URL(master, location.href)).toString() + '?t=' + t; }
    }
    if (variant) {
      var vr = await fetch(variant);
      await vr.text();
      await fetch('/hls/720/seg0.ts?t=' + t);
    }
  } catch (e) { console.error('fixture player', e); }
})();
""" % TOKEN


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass  # never leak tokens via the default access log

    def _send(self, status, ctype, body):
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_GET(self):
        path, _, query = self.path.partition("?")
        has_referer = "referer" in self.headers
        has_token = f"t={TOKEN}" in query

        if path.startswith("/hls/"):
            if not has_referer:
                time.sleep(0.6)  # let the page-hook body land first (race window)
                self._record(path, has_token, has_referer, 403)
                self._send(403, "text/plain", b"hotlink protection")
                return
            if not has_token:
                self._record(path, has_token, has_referer, 403)
                self._send(403, "text/plain", b"expired token")
                return

        if path == "/":
            self._record(path, has_token, has_referer, 200)
            self._send(200, "text/html", PAGE.encode())
        elif path == "/warm.html":
            self._record(path, has_token, has_referer, 200)
            self._send(200, "text/html", b"<html><body>warm</body></html>")
        elif path == "/player.html":
            self._record(path, has_token, has_referer, 200)
            self._send(200, "text/html", PLAYER.encode())
        elif path == "/player.js":
            self._record(path, has_token, has_referer, 200)
            self._send(200, "application/javascript", PLAYER_JS.encode())
        elif path == "/hls/master.m3u8":
            self._record(path, has_token, has_referer, 200)
            self._send(200, "application/vnd.apple.mpegurl", MASTER.encode())
        elif path in ("/hls/720/index.m3u8", "/hls/360/index.m3u8"):
            self._record(path, has_token, has_referer, 200)
            self._send(200, "application/vnd.apple.mpegurl", VARIANT.encode())
        elif path.startswith("/hls/720/seg"):
            self._record(path, has_token, has_referer, 200)
            self._send(200, "video/mp2t", SEG_BODY)
        else:
            self._record(path, has_token, has_referer, 404)
            self._send(404, "text/plain", b"not found")

    def _record(self, path, has_token, has_referer, status):
        with HITS_LOCK:
            HITS.append((path, has_token, has_referer, status))


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def redact(url):
    return re.sub(r"\?.*$", "?<redacted>", url or "")


def main():
    port = free_port()
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{port}"
    print(f"[1] fixture sunucu: {base} (token=***, referer gated)")

    from patchright.sync_api import sync_playwright

    profile = tempfile.mkdtemp(prefix="hazar-patchright-")
    checks = {}

    with sync_playwright() as p:
        ctx = p.chromium.launch_persistent_context(
            profile,
            executable_path=CHROME,
            headless=True,
            args=[f"--disable-extensions-except={EXT}", f"--load-extension={EXT}"],
        )
        print(f"[2] chromium: gecici profil {profile} (gercek profil kullanilmadi)")

        page = ctx.new_page()
        # Warm the MV3 service worker *before* the player page: on a fresh
        # profile the first request can otherwise land before listeners exist.
        page.goto(f"{base}/warm.html", wait_until="domcontentloaded")

        sw = None
        for _ in range(80):
            if ctx.service_workers:
                sw = ctx.service_workers[0]
                break
            page.wait_for_timeout(250)
        if not sw:
            print("[3] HATA: extension service worker yuklenmedi")
            ctx.close()
            server.shutdown()
            return 2
        ext_id = sw.url.split("/")[2]
        print(f"[3] extension yuklendi: {ext_id} (SW uzerinden dogrulanir)")

        page.goto(f"{base}/", wait_until="domcontentloaded")
        # MV3 service worker'i isinmis olsun; ilk yuklemede ilk istekler SW
        # uyanmadan once gidebilir. Ayni token'la reload edince webRequest
        # olayi kesin yakalanir.
        time.sleep(1.0)
        page.reload(wait_until="domcontentloaded")

        tab_id = sw.evaluate(
            "async (p) => { const t = (await chrome.tabs.query({})).find(x => x.url && x.url.startsWith(p)); return t ? t.id : null; }",
            base,
        )
        print(f"[4] fixture tabId: {tab_id}")

        # Oynatıcı segmentleri zamanla çeker; token'lı segment listesi oluşana
        # kadar kısa bir süre bekle (en fazla ~8s).
        raw = []
        for _ in range(16):
            time.sleep(0.5)
            raw = sw.evaluate(
                "(id) => candidatesFor(id).map(c => ({ url: c.url, kind: c.kind, "
                "isManifest: !!c.isManifest, isMaster: !!c.isMaster, expired: !!c.expired, "
                "encrypted: !!c.encrypted, segments: c.segments || [], frameId: c.frameId }))",
                tab_id,
            )
            media_ready = any(
                c["isManifest"]
                and not c["isMaster"]
                and c["segments"]
                and all("t=" in s for s in c["segments"])
                for c in raw
            )
            if media_ready and any(c["isMaster"] for c in raw):
                break

        print("[5] popup'un gordugu adaylar (candidatesFor):")
        media = None
        master = None
        for c in raw:
            label = "varyant" if c["isMaster"] else "segment"
            detail = " · ".join(
                x for x in [
                    c["kind"],
                    "playlist" if c["isManifest"] else None,
                    "baglanti suresi dolmus" if c["expired"] else None,
                    "tarayicida sifreli" if c["encrypted"] else None,
                    f"{len(c['segments'])} {label}" if c["segments"] else None,
                ] if x
            )
            tokenized = sum(1 for s in c["segments"] if "t=" in s)
            print(
                f"     {redact(c['url'])}  |  {detail}  |  frameId={c['frameId']}"
                f"  |  tokenli segment: {tokenized}/{len(c['segments'])}"
            )
            if c["isMaster"]:
                master = c
            elif c["isManifest"] and c["segments"]:
                media = c

        sw_log = sw.evaluate("() => (__hazarDebug().log || [])")
        dbg = sw.evaluate("() => ({ counters: __hazarDebug().counters, group_hls: __hazarDebug().settings.group_hls, buckets: __hazarDebug().segmentBuckets })")
        print("[6] extension log (ilgili satirlar):")
        print("     counters:", dbg["counters"], "group_hls:", dbg["group_hls"], "buckets:", dbg["buckets"])
        for line in sw_log:
            if "manifest" in line or "socket" in line:
                print("     ", line)

        # Fetch ONLY the first segment, through the player frame (same code path
        # `fetchInFrame` uses), and never hand bytes to the app.
        first_ok = False
        if media and media["frameId"] is not None and media["segments"]:
            first = media["segments"][0]
            result = sw.evaluate(
                """async (arg) => await new Promise((resolve) => {
                    chrome.tabs.sendMessage(
                        arg.tabId,
                        { type: 'fetch_bytes', url: arg.url },
                        { frameId: arg.frameId },
                        (r) => resolve(r || { error: 'no response' }),
                    );
                })""",
                {"tabId": tab_id, "url": first, "frameId": media["frameId"]},
            )
            first_ok = bool(result.get("ok")) and (result.get("bytes") or 0) > 0
            print(
                f"[7] ilk segment (oynatici frame'i {media['frameId']}): {redact(first)} "
                f"-> ok={result.get('ok')} bytes={result.get('bytes')} status={result.get('status')}"
            )
        else:
            print("[7] HATA: indirilebilir medya adayi bulunamadi")

        all_candidates = sw.evaluate(
            "async () => (await __hazarCandidates()).map(c => ({ url: c.url, tabId: c.tabId, "
            "expired: !!c.expired, status: c.status || null, isManifest: !!c.isManifest, "
            "isMaster: !!c.isMaster, segments: (c.segments||[]).length }))"
        )
        print("[6b] global __hazarCandidates():")
        for c in all_candidates:
            print(
                f"     tabId={c['tabId']} expired={c['expired']} status={c['status']} "
                f"manifest={c['isManifest']} master={c['isMaster']} segs={c['segments']} {redact(c['url'])}"
            )
        checks["no_false_expired"] = not any(c["expired"] for c in all_candidates)
        checks["no_phantom_tab"] = all(c["tabId"] >= 0 for c in all_candidates)
        checks["candidate_count"] = len(all_candidates) == 3
        checks["master_captured"] = master is not None  # fixture'da SW zamanlaması yarışı olabilir
        checks["master_labelled"] = master is None or bool(
            master["isMaster"]
            and not any(".ts" in s for s in master["segments"])
            and all(".m3u8" in s for s in master["segments"])
        )
        checks["media_tokenized"] = bool(
            media and media["segments"] and all("t=" in s for s in media["segments"])
        )
        checks["first_segment_ok"] = first_ok

        # Kullanıcının gördüğü hata: master adayı gönderilince manifest yeniden
        # fetch edilir, tek kullanımlık token yüzünden 403 → indirme başlamaz.
        # Düzeltme: manifest okunamazsa oynatıcının sniff'lediği segmentler kullanılır.
        sniffed = sw.evaluate("(id) => sniffedSegmentsFor(id)", tab_id)
        checks["sniffed_fallback"] = bool(
            sniffed and all("t=" in s for s in sniffed) and not any(".m3u8" in s for s in sniffed)
        )
        print(f"[7b] sniffedSegmentsFor -> {[redact(s) for s in sniffed]}")

        fake_master = f"{base}/hls/master.m3u8?t=EXPIRED"
        resolved = sw.evaluate(
            """async ([id, url, fid]) => {
                const r = await resolveSegments(id, url, fid, 3);
                return { error: r.error || null, segments: (r.segments || []).map(s => s),
                         segs: (r.segments || []).length };
            }""",
            [tab_id, fake_master, media["frameId"] if media else 6],
        )
        print(
            f"[7c] resolveSegments(suresi dolmus master) -> segs={resolved['segs']} "
            f"error={resolved['error']}"
        )
        checks["resolve_falls_back"] = bool(
            not resolved["error"]
            and resolved["segs"] > 0
            and all("t=" in s for s in resolved["segments"])
        )

        # "Hazar'a gönder" (app motoru) yolu: app'e manifest URL'i değil, gerçek
        # segment listesi verilmeli (yoksa app manifesti tekrar çekip 403 alır).
        via_grab = sw.evaluate(
            """async ([id, url, fid, variants]) => {
                const r = await segmentsForStream(id, url, fid, variants);
                if (!Array.isArray(r)) return { error: (r && r.error) || 'yok', segs: 0 };
                return { segs: r.length, all: r.every(s => s.includes('t=')),
                         anyManifest: r.some(s => /\.m3u8/.test(s)) };
            }""",
            [
                tab_id,
                master["url"] if master else fake_master,
                media["frameId"] if media else 6,
                master["segments"] if master else [],
            ],
        )
        print(f"[7d] segmentsForStream(grab yolu) -> {via_grab}")
        checks["grab_gets_real_segments"] = bool(
            via_grab.get("segs", 0) > 0 and via_grab.get("all") and not via_grab.get("anyManifest")
        )

        ctx.close()

    server.shutdown()
    server.server_close()

    segment_hits = [h for h in HITS if "/seg" in h[0]]
    print("[8] fixture istekleri (token degeri loglanmadi):")
    for path, has_token, has_referer, status in HITS:
        print(f"     {path}  token={'yes' if has_token else 'no'} referer={'yes' if has_referer else 'no'} -> {status}")

    print("\n[9] SONUC:")
    for name, value in checks.items():
        print(f"     {'OK  ' if value else 'FAIL'} {name}")
    ok = all(checks.values())
    print(f"\n     {'TUM KONTROLLER GECTI' if ok else 'BAZI KONTROLLER BASARISIZ'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
