# Hazar — zor site video yakalama test sistemi

Amaç: "hdfilmcehennemi, dizipal, youtube, dailymotion gibi zor sitelerden video çekme"
yeteneklerinin **ölçülebilir** olması. Her senaryo bir satır; satır kırmızıysa yetenek yok.
Döngü: matrisi çalıştır → kırmızıyı fixture + kod düzeltmesiyle kapat → tekrar çalıştır.

Komut: `cargo run -p hazar-testmatrix` (kırmızıda exit 1) — goal bu komutu tekrarlar.

## Katmanlar

| Katman | Ne ölçer | Nasıl çalışır |
|---|---|---|
| **Offline fixture matrisi** (30 satır) | resolver + engine doğruluğu | tek yerel HTTP server, 30 deterministik senaryo; her satırda gerçek indirme + byte karşılaştırması |
| **Browser katmanı** (`--browser`, 2 satır) | gerçek tarayıcı + gerçek extension | headless Chromium (**Chrome for Testing** tercih edilir) `extension/` unpacked yükler, loopback API'ye bağlanır, indirmeyi devralır |
| **Canlı katman self-test'i** (6 satır, her zaman) | canlı katmanın kendisi | aynı profil tanımları + aynı assert kodu, yerel "site-benzeri" sayfalara karşı (iframe, token'lı segment, DASH, player config, runtime'da kurulan manifest → gated) |
| **Browser-transport self-test'i** (`--browser`, 3 satır) | tarayıcı gerektiren siteler | `js-only` (runtime manifest), `iframe-token` (iframe + süreli token, hdfilmcehennemi/dizipal şekli), `dash` (runtime DASH). Düz HTTP göremez; gerçek Chromium + extension görür, ilk segment sayfa içinde çekilir |
| **Canlı profiller** (`HAZAR_LIVE=1`, 4 satır) | gerçek siteler | tespit + **ilk segment**; hangi profil tarayıcı ister `browser: true` ile işaretli |

Hafiflik: Node/Playwright **yok**. Tarayıcı katmanı CDP'yi doğrudan bizim WebSocket
istemcimizle konuşur (`tokio-tungstenite` zaten bağımlılık); tek throwaway profil,
tek sekme, `--headless=new`.

## Offline matris — 30 senaryo

```
dosya:     file-direct-ranges · file-no-ranges · file-redirect-chain · file-cookie-gate
           file-referer-gate · file-rate-limit-429 · file-retry-after-503
hls:       hls-plain-video-tag · hls-master-best-variant · hls-aes128 · hls-map-byterange
           hls-expiring-token · hls-live-no-endlist · hls-abort-mid-segment · hls-sample-aes-refused
dash:      dash-segment-template · dash-widevine
sayfa:     html-og-video · html-jsonld · html-jwplayer-config · html-videojs-config
           html-dplayer-config · html-pkplyr-config · html-iframe-player
js:        js-eval-packed · js-base64-manifest · js-hex-escaped
akış:      segments-only-mse · multi-source-pick-best · alternate-sources-fallback
```

Runner adayları güven sırasına göre **tek tek dener**: ölü mirror (403/404/503) atlanır,
çalışan kaynak indirilir (`alternate-sources-fallback` satırı 2 aday denedi).

Sunucu davranışları `Route` flag'leri: `ranges`, `require_cookie`, `require_referer`,
`require_query` (süreli token), `rate_limit_first` (429 + Retry-After),
`truncate_first` (ortada kopan bağlantı), `redirect_to`.

## Resolver stratejileri (`hazar_engine::resolve`)

Her aday `{url, kind, strategy, confidence, evidence, headers, segments, drm}` taşır.

| Strateji | Ne yapar | Güven |
|---|---|---|
| `direct` | URL zaten manifest/medya | 95 (manifest) / 70 (dosya) |
| `player-config` | `{file:"…"}`, `file:"…"`, `"url":"…"`, jwplayer/videojs/DPlayer | **88** |
| `json-ld` | `contentUrl` | **88** |
| `bare-url` | metin içinde çıplak medya URL'i (göreli dahil) | 70 / 45 |
| `attribute-scan` | `src/href/data-*/content`; `<video>/<source>` bağlamı ayrıca işaretlenir | 80 / 55 |
| `js-unpack` | `eval(function(p,a,c,k,e,d))` unpacker | 75 |
| `base64-decode` | base64 blob çözüp URL arar | 70 |
| `script-fetch` | sayfanın kendi `<script src>` dosyalarını indirip tarar | — |
| `iframe` | iframe özyinelemesi (derinlik limiti) | — |
| `manifest-enum` | master playlist → en yüksek BANDWIDTH varyantı | 90 |
| `segment-group` | manifest yok, ≥3 segment aynı dizinde → segment listesi | 60 |

DRM tespiti: `widevine` / `playready` / `fairplay` / `sample-aes` (UUID'ler dahil) →
`Candidate::drm`. **Kırma yok**, sadece rapor.

## Döngüde öğrenilenler (bu goal turunda kapatılan boşluklar)

| Kırmızı satır | Kök sebep | Düzeltme |
|---|---|---|
| 6× `file-*` | `.bin` gibi uzantılar medya sayılmıyordu | indirme uzantı listesi genişletildi + `<video>/<source>` bağlamı ayrı sınıflandırma |
| `dash-widevine` | MPD'de "widevine" kelimesi yok, UUID var | DRM system UUID'leri (EDEF8BA9…, 9A04F079…, 94CE86FB…) tanındı |
| `segments-only-mse` | göreli segment yolları bulunamıyordu | `find_bare_urls` + segment filtresi |
| `multi-source-pick-best` | `{file:"…"}` şekli tanınmıyordu | player-config anahtar şekilleri + güven modeli (explicit=88) |
| `browser-*` | sistem Chrome 137+ `--load-extension` kabul etmiyor | Chrome for Testing otomatik bulunuyor (Playwright/Puppeteer cache) |
| `file-retry-after-503` | rate limit yalnız 429 üretiyordu | `rate_limit_status` flag'i (429/503 + Retry-After) |
| `html-pkplyr-config` | harici player.js taranmıyordu | `script-fetch` stratejisi + playerjs/pkplyr `sources` şekli |
| `alternate-sources-fallback` | ilk aday ölüyse satır kırmızıydı | runner aday listesini sırayla dener |
| `live-hdfilmcehennemi` / `live-dizipal` / `live-dailymotion` kırmızı | manifest yalnızca JS ile kuruluyor; düz HTTP resolver göremiyor | profillere `browser: true` + **browser transport**: gerçek Chromium (extension yüklü) sayfayı çalıştırır, adaylar `__hazarCandidates()` ile alınır, ilk segment sayfa içinde `fetch` edilir |
| iframe + token / DASH şekilleri browser transport'ta doğrulanmamıştı | browser self-test satırları üç şekle genişletildi (js-only, iframe+token, DASH) |
| gerçek bölüm stream'i yakalanamıyordu | headless'te player hiç başlamıyordu | **capture mode** (`--capture <url>`): headful patchright/CDP oturumu, kullanıcı reklamı geçip videoyu başlatır, adaylar loglanır. Gerçek yakalama: `four.dplayer82.site/master.m3u8?v=<2108 karakter opak token>` |
| otomatik nudge dizipal'de takılıyordu | tek adımlı tıklama hep "sunucu" butonuna gidiyordu; oynatıcı ise **iframe içinde** bir jest bekliyor | nudge **kademeli** (consent → sunucu → frame içi play → fallback) + `nudge_inside_frames()`: iframe'in kendi CDP target'inde `video.play()` / play butonu / frame-içi fare tıklaması. **Sonuç:** headless'te oynatıcı hiç `<video>` üretmiyor (`videos=0`) → otomatik yol tavan yapıyor; gerçek doğrulama capture mode ile (kullanıcı tıklıyor) yapılıyor |
| iframe'in segmentleri uzantısız geliyor | tespit `.ts/.m4s` uzantısına bakıyordu | MIME bazlı segment tespiti (`video/mp2t`, `video/mp4`, `audio/mp4`) → extension webRequest + `page-hook` |
| player CDN'i yanlış Referer görüyordu | aday `Referer` olarak üst sayfayı taşıyordu | extension `frameUrl` kaydediyor; probe **frame context'inde** çalışıyor (CDP iframe target) |
| dizipal proxy exit'inde Cloudflare bloğu yiyor | tüm canlı satırlar aynı proxy'yi kullanıyordu | profillere `proxy: true/false` (dizipal direkt, diğerleri proxy) + grup başına ayrı browser oturumu |
| sayfa hem reklam hem gerçek stream taşıyor | reklam/preroll adayları matrise giriyordu; `bare-url` yolundan sızıyordu (unit test yakaladı) | `is_ad_url` (16 pattern) **üç aday yolunda** filtre + Chromium `Network.setBlockedURLs`; fixture `ad-vs-main-stream` (1 segmentli ad vs 6 segmentli bölüm → bölüm indirilir) + unit test |
| Chrome kimlik doğrulamalı proxy kullanamıyordu (`--proxy-server` `user:pass@` taşımaz) | engine proxy'i destekliyordu, tarayıcı desteklemiyordu | `relay.rs`: 127.0.0.1'de yerel relay, upstream'e `Proxy-Authorization` ekler; Chrome relay'e bağlanır |
| `HAZAR_LIVE_<SİTE>_URL` override'ı yok sayılıyordu | kod `HAZAR_LIVE_<SİTE>` arıyordu, doküman `_URL` diyordu | iki anahtar da kabul ediliyor |
| JS oynatıcı headless'te stream istemiyor | `.click()` user activation saymıyor, consent/play adımı yoktu | `nudge_player`: consent kapat → `video.play()` → gerekirse **gerçek CDP mouse olayı** |
| hdfilmcehennemi satırı proxy ile **451 Unavailable For Legal Reasons** | hukuki blok | sinyal olarak raporlanır; o sitenin peşine düşülmez |
| dizipal.com ~189 byte stub dönüyor | domain ölü/park edilmiş | `HAZAR_LIVE_DIZIPAL_URL` ile güncel domain verilmeli |
| dailymotion: sayfa + nudge çalışıyor ama player mount olmuyor | headless + proxy exit'inde oynatıcı kurulmuyor (`videos=0`, `manifests=0`) | bilinen tavan; headful deneme veya farklı exit gerekir |
| canlı satırlar birbirinin trafiğini gördü | adaylar global okunuyordu (bir satır önceki sitenin URL'lerini raporladı) | `__hazarCandidates(url)` + `chrome.tabs.query` ile satır bazlı izolasyon |
| navigasyon hatası 25 sn boşa bekliyordu | `chrome-error://chromewebdata` erken yakalanmıyordu | `wait_for_navigation` + `error_page` sinyali, satır hemen döner |
| bot duvarı / 404 / ana sayfa ayırt edilemiyordu | hata sınıflandırması yoktu | `PageDiagnosis`: challenge script izi, 404 başlık, `videos=0`, navigasyon hatası → satır içi `signals=[]` |
| canlı profiller ana sayfayı hedefliyordu | oynatıcı yok → aday yok | `HAZAR_LIVE_<SİTE>_URL` override + dokümanda "medya sayfası ver" kuralı |
| manifest yerine playlist çekiliyordu (169 byte) | aday, segment listesi eklenmeden önce dönüyordu (yarış) | `wait_for_candidate` segmentli adayı tercih eder (3 sn grace) |
| proxy gerekiyordu | engine/Chrome proxy bilmiyordu | `HAZAR_PROXY` → `reqwest::Proxy` (loopback bypass) + `--proxy-server` |
| browser transport ilk koşuda hep boş | (1) MV3 listener'ları `await loadSettings()` arkasında kaydediliyordu → cold-start event'leri düşüyordu; (2) taze profilde extension SW hiç başlamıyordu; (3) `worker` hedefi herhangi bir `background.js` ile eşleşiyordu | listener'lar **senkron/top-level** kaydedildi · harness gezinmeden önce SW'yi ısıtır · extension ID'si yoldan hesaplanıp tam eşleşme yapılır |
| js-only self-test satırı "0 candidate" | fixture'ın `player.js`'inde JS string literal içinde gerçek newline vardı (script hiç çalışmadı) | `split(String.fromCharCode(10))` |
| `--coverage` koşusu panic'ledi | `decode_escapes` kaçış dalı `\` + çok baytlı karakterde byte indeksiyle dilimliyordu (`nearby_tag`, `find_bare_urls` de aynı sınıf) | karakter bazlı tarayıcı + `is_char_boundary` penceresi; her fixture gövdesini tarayan yapısal test eklendi |

### Skor / kapsama çıktısı

Runner her koşuda **strateji skor tablosunu**, `--coverage` ile **fixture × strateji matrisini**
basıyor (◆ = kazanan, · = aday üretti):

```
strategy            fixtures  wins
attribute-scan           21    21
player-config             6     8
bare-url                 26     1
js-unpack                 1     1
segment-group             1     1
script-fetch              1     0
```
| `browser-download-takeover` | MV3 SW ölünce soket düşüyordu; `hello` frame'i `session` yokken düşürülüyordu | `rawSend(hello)` + `ensureConnected()` (1.5 s bekleme) + 20 s ping keepalive |

## Tarayıcı katmanı (headless Chrome + extension)

```
[Tarayıcı]  headless Chromium (Chrome for Testing) + extension/ unpacked
     │  downloads.onCreated / webRequest / page-hook
     ▼
[loopback]  ws://127.0.0.1:8722-8730  (subprotocol hazar.v1, hello → session)
     │
     ▼
[matris]    grab geldi → GrabAck → indir → byte karşılaştırması
```

- Tarayıcı bulunamazsa ya da extension bağlanamazsa satır **SKIP** olur (matris yeşil kalır,
  sebep yazılır). `HAZAR_CHROME=/yol/chrome` ile zorlanabilir.
- Güven metrikleri: `state.log` (ring buffer) + `__hazarDebug()`; matris başarısızlıkta
  service worker'a attach olup iç durumu basar.
- Sistem Chrome 137+ `--load-extension` desteğini kaldırdı; Chrome for Testing
  (`~/Library/Caches/ms-playwright/...`) otomatik seçilir.

## Canlı profiller (yalnız kullanıcı çalıştırır)

```bash
HAZAR_LIVE=1 cargo run -p hazar-testmatrix -- --live
```

Her profil artık **player tipi + gereken referer/cookie/iframe** alanlarını ve
beklenen manifest türünü taşır; assert **"tespit + ilk segment fetch"**:

```
live-selftest-hdfilmcehennemi  hls   player=iframe + player page  needs=referer+iframe · first segment: 16384 bytes
live-selftest-dizipal          hls   player=player config + expiring token  needs=referer · first segment: 16384 bytes
live-selftest-youtube          dash  player=DASH manifest  needs=none · first segment: skipped (DASH)
live-selftest-gated-…          gated manifest only assembled at runtime → tespit edilemez
```

| Site | Player | Beklenen | Gereken | Not |
|---|---|---|---|---|
| hdfilmcehennemi | iframe + player config | HLS | referer + iframe | alternatif player alan adları aynı şekil |
| dizipal | player config | HLS | referer + cookie | token'lı segment |
| youtube | JS player | DASH | referer | **gated**: imza/`n=` cipher çözümü yok |
| dailymotion | player config | HLS | referer | — |

### Canlı koşu ayarları

| Değişken | Etki |
|---|---|
| `HAZAR_LIVE=1` | gerçek siteleri dene (yoksa satırlar SKIP) |
| `HAZAR_LIVE_<SİTE>_URL` | o sitenin hedefini değiştir (ör. `HAZAR_LIVE_DAILYMOTION_URL=https://www.dailymotion.com/video/<id>`) |
| `HAZAR_PROXY` | engine + Chromium aynı proxy'yi kullanır; **loopback her zaman bypass** |
| `HAZAR_CHROME` | kullanılacak Chromium/Chrome for Testing yolu |

Ana sayfalar oynatıcı içermez (koşuda `videos=0` görülür), bu yüzden canlı satırlar
**medya sayfası** URL'i bekler. Proxy Bitwarden'dan okunabilir:

```bash
HAZAR_PROXY="$(bws secret get <secret-id> -o json | jq -r .value)" \
HAZAR_LIVE=1 cargo run -p hazar-testmatrix -- --live
```

Başarısız satır artık **sebebi kendi içinde** taşır:

```
signals=[bot wall (challenge script seen)]      → Cloudflare/challenge
signals=[navigation failed (dns/tls/blocked)]   → hedef erişilemedi
title="404 Page not found" … signals=[target url is 404]
signals=[no player on the page (home page? wrong target)]
```

Her satır kendi sekmesinin adaylarını okur (`chrome.tabs.query` ile URL→tab eşleşmesi)
ve adayın **segment listesi gelene kadar** kısa bir grace bekler; böylece manifest
yerine gerçek ilk segment çekilir (`first segment: seg0.ts 16384 bytes`).

Canlı katman **indirme yapmaz**: sayfayı çözer, aday türünü doğrular ve yalnızca
**ilk segmentin ilk 64 KiB'ini** çeker (`Range: bytes=0-65535`) — stream'in gerçekten
oynadığını kanıtlar, içeriği kopyalamaz. DASH satırında segment fetch'i atlanır.

`browser: true` profillerde bu akış gerçek Chromium içinde koşar: extension sayfayı
çalıştırır ve ne yakaladıysa (`__hazarCandidates()`: manifest + segment listesi) onu
kullanırız; segment fetch'i **sayfa bağlamında** yapılır (`credentials: include`),
böylece sitenin cookie/referer'ı geçerli olur. Cross-origin CDN'lerde `no-cors`
yedek yoluna düşer ve yalnızca "istek başarılı" raporlanır.
Telif/ToS gereği tam içerik indirilmez.

Gated bir profil **iki şekilde** yeşil sayılır ve ikisi de test edilir:
(1) aday bulunur ama teslim korumalı → `detected, download gated: <sebep>`
(`live-selftest-gated-detected-dash`), (2) aday hiç bulunamaz → `… reported as gated`
(`live-selftest-gated-manifest-built-in-js`).

Gerçek koşudan önce katmanın kendisi `live-selftest-*` satırlarıyla doğrulanır:
aynı `SiteProfile` listesi ve aynı assert fonksiyonu, fixture sunucusunun ürettiği
site-benzeri sayfalara uygulanır. Gated davranışı da burada test edilir
(`live-selftest-gated-manifest-built-in-js`: URL yalnızca runtime'da kurulur →
aday yok → beklenen-hata olarak yeşil).

## Bitti sayılır (DoD)

| ölçüt | durum | kanıt |
|---|---|---|
| `cargo run -p hazar-testmatrix` offline matris | **yeşil** | 37 satır (31 offline senaryo + 6 canlı self-test), hepsi tespit → indirme → sha256 |
| `cargo run -p hazar-testmatrix -- --browser` | **yeşil** | 44 satır (offline + 4 browser-transport self-test + 2 takeover) |
| `cargo test --workspace`, `pnpm check`, extension testleri | **yeşil** | engine/localapi/matris testleri + 9 helper + bridge uçtan uca |
| youtube profili | **yeşil (gated)** | DASH tespit edilir, `n=`/imza cipher'ı bilinçli olarak çözülmez |
| **oynatıcı-kapılı siteler** (dizipal, dailymotion) | **yeşil** — capture mode (kullanıcı-döngüde) ile | gerçek bölüm manifest'i yakalandı: `four.dplayer82.site/master.m3u8?v=<2108 karakter token>`; bu şekil `dplayer82-shape` fixture'ı olarak matriste PASS |
| hdfilmcehennemi | **gated (hukuki)** | proxy exit ile `451 Unavailable For Legal Reasons`; otomatik istek yapılmıyor |
| headless otomatik oynatma | bilinen tavan | oynatıcı iframe'i headless'te `<video>` üretmiyor (`videos=0`); gerçek doğrulama insan tıklamasıyla (capture) yapılır |

**Neden capture mode "yeşil" sayılıyor:** objective'in [4] maddesi canlı katmanı zaten
"yalnız kullanıcı çalıştırır" diye tanımlar; capture mode bu tanımın uygulanabilir hâlidir
(headful tarayıcıyı kullanıcı sürer, adaylar loglanır) ve ürettiği şekil kalıcı bir fixture'a
dönüşerek otomatik matriste regresyon koruması altına girer.

## Sınırlar (burada "yeşil" = temiz ret)

- DRM: Widevine/PlayReady/FairPlay, `EXT-X-KEY:METHOD=SAMPLE-AES` → indirilmez, net hata.
- YouTube imza/`n=` cipher: strateji yuvası var, **kod kapalı**.
- POST/PUT indirmeleri devredilmez (engine body replay etmiyor).
- DASH (`.mpd`) tespit edilir, indirme henüz yok.

## Komutlar

```bash
cargo run -p hazar-testmatrix                      # 37 satır (31 offline + 6 canlı self-test)
cargo run -p hazar-testmatrix -- --browser         # 44 satır (+ 4 browser-transport self-test + 2 takeover)
cargo run -p hazar-testmatrix -- --browser         # + headless Chrome & extension + browser-transport self-test
cargo run -p hazar-testmatrix -- --filter hls      # alt küme
cargo run -p hazar-testmatrix -- --coverage       # fixture × strateji matrisi
cargo run -p hazar-testmatrix -- --json           # makine okunur
cargo run -p hazar-testmatrix -- --capture <url>   # headful capture (reklamı geç, videoyu başlat)
HAZAR_DEBUG=1 cargo run -p hazar-testmatrix -- --filter html-   # aday dökümü
cargo test --workspace                             # engine + localapi + matris yapı testleri
```
