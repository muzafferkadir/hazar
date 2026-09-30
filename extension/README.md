# Hazar Integration — kurulum (unpacked)

Hazar'ın tarayıcı eklentisi. Chrome Web Store / AMO'da **yayınlanmıyor**; klasör
olarak verilir ve tarayıcıya "unpacked" yüklenir.

## 1) Eklentiyi indir

Release sayfasından `artifacts/hazar-extension-vX.Y.Z.zip` dosyasını indir ve bir klasöre çıkar
(örn. `~/hazar-extension`). Klasörün içinde `manifest.json` **doğrudan** görünmeli —
yani zip'i çıkarınca `hazar-extension/manifest.json`, `hazar-extension/src/...` olsun.

> Repodan kullanıyorsan: `extension/` klasörünün kendisi yeterli, zip gerekmez.

## 2) Hazar uygulamasını aç

- macOS: `Hazar.app` (DMG'den kurdun) · Windows: kurulumdan sonra **Hazar**
- Uygulama açılınca köprü `127.0.0.1:8722-8730` üzerinde dinlemeye başlar.
- Diğer indirmelerin devam etmesi için pencereyi kapatabilirsin; uygulama menü
  çubuğunda/tray'de kalır (macOS: menü çubuğu → **Aç / Çıkış**).

## 3) Tarayıcıya yükle

**Chrome / Edge / Brave / Vivaldi**
1. `chrome://extensions` (Edge: `edge://extensions`)
2. Sağ üstte **Geliştirici modu**nu aç
3. **Paketlenmemiş öğe yükle** → çıkardığın `hazar-extension` klasörünü seç

**Firefox**
1. Klasörün bir kopyasını al (Chrome'a yüklediğin klasörü bozma)
2. Kopyada `manifest.firefox.json` dosyasını `manifest.json` üzerine kopyala
   (Chrome MV3 `background.scripts` anahtarını reddediyor, Firefox ise
   `background.service_worker`'ı desteklemiyor — bu yüzden iki manifest ayrı duruyor.)
3. `about:debugging#/runtime/this-firefox` → **Geçici Eklenti Yükle…** → `manifest.json`
   (Firefox geçici eklentiyi kapanışta unutur; her oturumda tekrar yükle.)

## 4) Çalıştığını doğrula

Eklenti simgesine tıkla: durum **"bağlı · :8722"** olmalı. Değilse **Yeniden bağlan**.
Uygulama kapalıysa "Hazar açık değil" yazar ve indirmeler tarayıcıda kalır (kaybolmaz).

## 5) Güncelleme

Eklenti tarayıcı tarayıcı elle güncellenir: yeni zip'i indir, aynı klasöre çıkar
(üzerine yaz), sonra `chrome://extensions` → eklenti kartında **Yenile**.
Uygulama (Hazar.app) kendi kendini günceller (updater `latest.json` okur).

## Yakalanan stream inmediyse

Bazı oynatıcılar manifest'i **tek kullanımlık token** ile ve/veya **tarayıcıda şifreli**
şekilde servis eder. Eklenti bu durumda şunu yapar:

1. Oynatıcının kendi `m3u8` isteğinin **yanıt gövdesini** yakalar (`page-hook`).
2. Gövde düz metin (`#EXTM3U`) ise **segment listesini** çıkarır → popup'ta
   **"Segmentleri indir (N)"** görünür. Bu yolu kullan: app manifest'i tekrar istemez,
   segmentleri doğrudan indirir (tek kullanımlık token sorunu ortadan kalkar).
3. Gövde playlist değilse (HTML/şifreli) aday **"indirilemez (şifreli/süresi dolmuş)"**
   olarak işaretlenir ve buton kapalıdır — o stream'i ancak oynatıcının kendi JS'i çözer.
4. Manifest isteği `404` dönerse (token tüketilmiş) aday "bağlantı süresi dolmuş" olur.

**Pratik akış:** videoyu oynatmaya başla (oynatıcı segmentleri istemeye başlar) → eklenti
simgesine tıkla → aday "Segmentleri indir (N)" ise onu kullan; "indirilemez" ise o sitenin
stream'i client-side şifreli demektir (indirme kapsam dışı).

## Sorun giderme

| belirti | çözüm |
|---|---|
| popup'ta "Hazar açık değil" | Uygulamayı aç; popup'ta **Yeniden bağlan** |
| indirme hâlâ tarayıcıda iniyor | Options → "İndirmeleri yakala" açık mı; site `excluded_hosts` listesinde mi; boyut `min_size_bytes` altında mı |
| yeni sekmede açılan oynatıcı | eklenti tüm çerçeveleri izler (`all_frames`), ama bazı siteler anti-DevTools/anti-otomasyon kullanır → uygulama capture mode ile doğrulanır |
| Firefox: eklenti kayboldu | geçici eklenti; `about:debugging`'den tekrar yükle |
| Chrome: `'background.scripts' requires manifest version of 2 or lower` | `manifest.firefox.json`'u `manifest.json` üzerine kopyalamışsın; Chrome için orijinal `manifest.json`'u kullan |
| "Bu sayfadaki medya" boş | eklentiyi `chrome://extensions` → **Yenile** ile güncelle (eski popup sürümü hatası) |

---

# Teknik notlar (geliştirici)

Protokol, yakalama yolları ve testler aşağıda. (Ayrıntılı test planı:
`../docs/CAPTURE-TESTPLAN.md`)

## Yakalama yolları

| yol | mekanizma |
|---|---|
| Tarayıcı indirmesi | `downloads.onCreated` → karar → `downloads.cancel` + uygulamaya `grab`; uygulama onaylamazsa `downloads.resume` |
| Ağ başlıkları | `webRequest.onBeforeSendHeaders` (Cookie/Referer/UA) + `onHeadersReceived` (Content-Type/Disposition/Length) |
| POST ile gelen indirme | tespit edilir ama devredilmez (engine body replay etmiyor) |
| HLS/DASH manifest | `.m3u8`/`.mpd` görülür, playlist bir kez çekilip segment listesi uygulamaya verilir |
| MSE / tokensız segmentler | `page-hook.js` XHR/fetch'i izler; **MIME** (`video/mp2t` vb.) ile uzantısız segmentler de yakalanır |
| Sayfa medyası | `content.js`: `<video>/<audio>/<source>/<embed>/<object>` + `og:video`/`twitter:player` |
| Context menu | "Download with Hazar" |
| Yeniden yakalama | `declarativeNetRequest` session rule → `recapture.html#<url>` → taze cookie ile tekrar `grab` |
| Reklam filtresi | `is_ad_url` (engine) + Chromium `Network.setBlockedURLs` (test/browser katmanı) |

## Protokol (named JSON, `hazar.v1`)

WebSocket: `ws://127.0.0.1:8722` (8722–8730 arası denenir), subprotocol `hazar.v1`.
İlk mesaj `hello` olmalı; uygulama bağlantıya özel `session` döner.

```json
{"type":"hello","protocol":1,"client":"chrome","extension_id":"…","version":"0.1.0"}
{"type":"grab","session":"…","id":"…","request":{
  "url":"…","kind":"file|hls|dash","filename":"…","mime":"…","size":null,
  "method":"GET","referer":"…","user_agent":"…","cookie":"a=b; c=d",
  "headers":[["Authorization","Bearer …"]],"page_url":"…","frame_url":"…","tab_id":7,
  "segments":["…"],"manifest":null,"connections":null,"expected_sha256":null,"speed_limit_bps":null}}
{"type":"cancel","session":"…","id":"…"}
{"type":"media","session":"…","tab_id":7,"items":[…]}
{"type":"ping","session":"…","t":1712345678}
```

Uygulama → eklenti: `hello_ok`, `hello_err`, `grab_ack`, `progress`, `finished`,
`failed`, `queue`, `settings`, `pong`, `error`.

Kimlik doğrulama: yalnız loopback + zorunlu subprotocol + `hello` + bağlantıya özel
session token.

## Geliştirme / test

```bash
node extension/test/lib.test.cjs       # saf yardımcılar (tarayıcısız)
node extension/test/bridge.test.cjs    # gerçek Rust app + WebSocket uçtan uca
pnpm check:extension                   # node --check ile syntax
```

## Bilinen sınırlar

- POST/PUT indirmeleri devredilmiyor (engine body replay etmiyor).
- DASH `type="dynamic"` (canlı) manifestler reddedilir.
- Client-side şifreli playlist'ler (ör. `master.m3u8?v=<token>` yanıtı `text/html`)
  net hata ile reddedilir: `manifest is not an HLS playlist (…)`.
- Widevine/DRM ve YouTube `n=` cipher: kod yok (tespit + temiz ret).
- Safari yok (ayrı Safari App Extension gerekir).
