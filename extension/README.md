# Hazar Integration (browser extension)

MV3 extension for Chrome, Edge and Firefox. It hands downloads to the Hazar app
over a loopback WebSocket and sniffs HLS/DASH streams.

## Kurulum (unpacked)

1. Chrome/Edge: `chrome://extensions` → Developer mode → **Load unpacked** → `extension/` dizinini seç.
2. Firefox: `about:debugging#/runtime/this-firefox` → **Load Temporary Add-on** → `extension/manifest.json`.
3. Hazar uygulamasını aç (köprü `127.0.0.1:8722-8730` aralığında dinler).
4. Popup'ta durum **"bağlı · :8722"** olmalı. Değilse **Yeniden bağlan**.

Store yayını için: `web-ext lint` (Firefox) / Chrome Web Store zip'i (`extension/` klasörünü zip'le).

## Ne yapıyor

| Yakalama yolu | Mekanizma |
|---|---|
| Tarayıcı indirmesi | `downloads.onCreated` → karar → `downloads.cancel` + app'e `grab`; app ack vermezse `downloads.resume` |
| Ağ başlıkları | `webRequest.onBeforeSendHeaders` (Cookie/Referer/UA) + `onHeadersReceived` (Content-Type/Disposition/Length) |
| POST ile gelen indirme | Tespit edilir ama **devredilmez** (engine POST body replay etmiyor) — tarayıcıda kalır |
| HLS/DASH manifest | `.m3u8`/`.mpd` isteği görülür, playlist bir kez `fetch` edilip segment listesi app'e birlikte verilir |
| MSE (blob) streamleri | `page-hook.js` sayfa context'inde XHR/fetch'i izler, segment URL'lerini toplar; aynı dizindeki ≥3 segment "stream" sayılır |
| Sayfa medyası | `content.js`: `<video>/<audio>/<source>/<embed>/<object>` + `og:video`/`twitter:player` meta taraması |
| Context menu | "Download with Hazar" (link/video/audio/image) |
| Yeniden yakalama | `declarativeNetRequest` session rule → `recapture.html#<url>` → taze cookie/session ile tekrar `grab` (IDM'in `captured.html` mekaniğinin karşılığı) |

## Protokol

Named JSON, `crates/hazar-localapi` ile birebir. WebSocket subprotocol: `hazar.v1`.

Extension → app:

```json
{"type":"hello","protocol":1,"client":"chrome","extension_id":"…","version":"0.1.0"}
{"type":"grab","session":"…","id":"…","request":{ "url":"…","kind":"file|hls|dash","filename":"…",
  "mime":"…","size":null,"method":"GET","referer":"…","user_agent":"…","cookie":"a=b; c=d",
  "headers":[["Authorization","Bearer …"]],"page_url":"…","tab_id":7,
  "segments":["…"],"manifest":null }}
{"type":"cancel","session":"…","id":"…"}
{"type":"media","session":"…","tab_id":7,"items":[…]}
{"type":"ping","session":"…","t":1712345678}
```

App → extension:

```json
{"type":"hello_ok","protocol":1,"app":"Hazar","version":"0.1.0","session":"…",
 "features":["grab","cancel","progress","hls"],"settings":{"connections":8,…}}
{"type":"hello_err","reason":"protocol mismatch"}
{"type":"grab_ack","id":"…","state":"downloading"}
{"type":"progress","id":"…","phase":"download","written":123,"total":456,"connections":8,"speed_bps":0}
{"type":"finished","id":"…","path":"/Users/…/file.zip","size":456,"sha256":null,"elapsed_ms":1234}
{"type":"failed","id":"…","reason":"…"}
{"type":"pong","t":1712345678}
```

Kimlik doğrulama: bağlantı yalnız loopback'e, subprotocol zorunlu, ilk mesaj `hello`
olmalı ve app her bağlantıya özel bir `session` token'ı döner; sonraki mesajlarda
bu token kontrol edilir. (IDM'in `?cid=&rnd=` handshake'inin isimli/JSON karşılığı.)

## Ayarlar

Popup ve options sayfası `chrome.storage.local` kullanır:

- `capture_enabled` — hiç yakalama yapma
- `capture_manifests` — HLS/DASH streamlerini de yakala
- `group_hls` — segmentleri tek dosyada birleştir
- `min_size_bytes` — altındaki indirmeler tarayıcıda kalır (varsayılan 512 KB)
- `excluded_hosts` — `*.example.com` biçiminde host listesi
- `deny_patterns` — URL regex listesi
- `ports` — app aranacak port listesi

## Geliştirme / test

```bash
node extension/test/lib.test.cjs       # saf yardımcılar (tarayıcısız)
node extension/test/bridge.test.cjs    # gerçek Rust app + WebSocket uçtan uca
# veya
pnpm test:extension
pnpm test:bridge
pnpm check:extension                   # node --check ile syntax
```

`bridge.test.cjs` gerçek `hazar-localapi` server'ını ayağa kaldırır
(`cargo run --example echo_server`), extension'ın ürettiği `grab` payload'ının
Rust tarafında birebir çözüldüğünü doğrular.

## Bilinen sınırlar

- POST/PUT ile başlatılan indirmeler devredilmiyor (engine body replay desteklemiyor).
- DASH (`.mpd`) yakalanır ama indirme henüz yok; app `failed` + açıklama döner.
- safari yok (ayrı Safari App Extension gerekir).
- `blob:` URL'ler tespit edilir, indirilemez (segment listesi varsa o kullanılır).
- Widevine/DRM korumalı streamler indirilemez (bilinçli olarak kapsam dışı).
