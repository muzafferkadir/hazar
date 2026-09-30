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

## yt-dlp session ve network (0.2.5)

- Cookie jar, Referer/User-Agent ve özel captured header'lar ile fresh context. Header'lar origin ile sınırlıdır; cross-origin redirect'te auth taşınmaz. YouTube header/auth üretimi yt-dlp'ye bırakılır.
- Chrome fixed proxy ayarı okunur; PAC/auto-detect manuel URL ister. Network ayarları extension options içindeki kapalı details bölümündedir. Proxy/source IP eşlemesi gerçek çıkış IP'sinin eşit olduğunun kanıtı değildir. Farklı host/protokol bypass kuralları olan proxy için açık URL tercih edilmelidir. Browser proxy credential challenge aktarılmaz.
- Site bazlı Chrome impersonation ve browser'da gözlenen Cloudflare 403 için impersonation seçimi. User-Agent tek başına TLS fingerprint değildir.
- bgutil 2.0.0 script provider, bundled Deno ve platform canvas addon'ları. Yalnız paketli plugin path açık; HTTP provider çıkarıldı. Token client/session/video ID bağlama upstream provider API'sine bırakılır. Cache per-job geçici dizinde tutulur. Token üretimi/site başarısı ayrıca canlı doğrulanmalıdır.
- YouTube guest önce denenir; login gerektiğinde mevcut account cookie ile en fazla bir retry. Rate limit otomatik tekrar döngüsü doğurmaz.
- Probe hataları popup'ta görünür; manuel tekrar analiz, 429 için cooldown. Cookie/proxy ayarı değişimi ve sayfa yenileme ilgili probe cache'ini yeniler.

## yt-dlp alternatifi

0.2.4+ app `ytdlp` capability’si verir. Popup açılınca veya görünür video bulunduğunda sayfanın metadata'sı app içindeki yt-dlp ile analiz edilir. İndirilebilir video bulunursa normal Hazar adaylarına ek olarak kırmızı `yt-dlp` adayı gösterilir. Tıklanan seçenek hangi engine'in kullanılacağını belirler; otomatik fallback yoktur. yt-dlp için MP4 video output seçilir; audio mevcutsa mux edilir. YouTube için video ve audio birlikte zorunludur.

Extension cookie API üzerinden ilgili sayfa ve gözlenmiş player frame domain'lerinin cookie'lerini alır. Domain/path/secure/expiry/HttpOnly metadata korunur; app özel geçici cookie jar ve Referer/User-Agent ile probe/download process'ine aktarır. Cookie jar normal tamamlanma, hata ve cancellation sonunda silinir. Cookie değerleri queue JSON'una yazılmaz; browser DB okunmaz. Download başlangıcında fresh context istenir. Tab kapanmış/değişmiş veya app restart sonrası session ownership kaybolmuşsa extension’dan yeniden gönderilmelidir. Chrome 132+ frame partition API varsa seçilmiş frame partition’ı job’a özel jar’a aktarılır; eski browser/Firefox aynı API’yi sağlamayabilir. Yeni domain'e yönlenen extractor'ın ek oturuma ihtiyacı olabilir; DRM, live ve playlist kapsam dışıdır.

Probe sonucu kısa süre cache'lenir. Site değişiklikleri/login/PO token/anti-bot koşulları yüzünden yt-dlp adayının download'u yine hata verebilir.

## Video paneli ve toplu linkler

Video tespit edilince player'ın sağ üstünde **Hazar ile indir** çıkar. Birden fazla kalite varsa buton kalite menüsünü açar. HLS master'ın AUDIO grubu seçilen kaliteye göre çözülür; ayrı audio/video segmentleri browser oturumundan app'e aktarılır ve FFmpeg ile mux edilir. App'in `hls_audio_bytes` capability'si gerekir (0.2.2+).

Panel her iframe'in kendi content script'inde çalışır. Source eşleşmeyen birden fazla player varsa yalnız tek aktif player'a network adayları bağlanır. Scroll, resize, DOM source değişimi ve fullscreen izlenir. Native video-element fullscreen overlay göstermez; player container fullscreen desteklenir. DOM `<track>` subtitle/caption linkleri menüye eklenir; HLS subtitle rendition indirme henüz yoktur.

Sağ tık menüsünden sayfadaki veya seçili download linkleri gönderilebilir. Yalnız `download` attribute'lu veya bilinen dosya uzantılı HTTP/HTTPS linkler alınır; en fazla 200 link, tekilleştirilerek mevcut queue'ya gönderilir. HTML sayfaları toplu download'a alınmaz.

Browser download devralınırken önce pause yapılır. App ACK verirse browser kaydı cancel/erase edilir; ACK yoksa browser download resume edilir.

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

**En sağlam yol — "Sayfada indir":** segmentler elimizdeyse popup'ta
**"Sayfada indir (N)"** butonu çıkar. Eklenti segmentleri **oynatıcının kendi
frame'inde** indirir (Referer/çerez oynatıcınınkiyle aynı olur) ve baytları app'e
aktarır; app parçaları birleştirip dosyayı yazar. Böylece app'in URL'i dışarıdan
tekrar istemesi (tek kullanımlık token → 403/404) tamamen atlanır. IDM'in kernel
driver'ı ile yaptığı işin tarayıcı içi karşılığı budur.

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
- Widevine/DRM: destek yok; YouTube signature çözümleme paketli yt-dlp/Deno ile yapılır.
- Safari yok (ayrı Safari App Extension gerekir).
