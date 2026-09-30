# IDM → Hazar: eksikler ve uygulama planı

Tarih: 2026-09-30. İncelenen durum: mevcut working tree, uncommitted değişiklikler dahil.

## v0.2.0 uygulama notu

Bu rapor değişiklik öncesi incelemedir. v0.2.0: Range/validator kontrolü, staging publish, worker work queue, kalıcı queue/settings, pause/resume/link refresh, desktop DASH + typed XML, HLS captured body/alternate audio, FFmpeg mux, session/origin/ownership ve disk ACK uygulandı. SQLite yerine atomik dosya state’i kullanıldı. Aktif range split, native messaging, store/signing, live recording ve yt-dlp eklenmedi; güncel kapsam README ve PLAN’dadır.

## Kapsam ve kanıt düzeyi

IDM'nin resmi özellik/support sayfaları, yerel `~/Dev/re/idm/ANALYSIS.md` ve `EXTENSION.md`, Hazar engine/extension/bridge/UI/release workflow incelendi. Yerel IDM raporu 6.43 build 11'e ait; bugünkü IDM sürümünün runtime davranışını kanıtlamaz. Binary stringlerinden yapılan çıkarımlar, resmi belgede açıklanan davranışlardan ayrı değerlendirilmelidir.

Bu inceleme statiktir. IDM Windows VM'de çalıştırılmadı; Hazar uygulaması başlatılmadı; test veya performans benchmark çalıştırılmadı. Aşağıdaki bug etkileri kod akışından çıkarılmıştır. Hız üstünlüğü veya gerçek site başarı oranı ölçülmüş değildir.

## 1. IDM'nin fark yaratan davranışları

| Alan | IDM davranışı | Hazar için sonuç |
|---|---|---|
| Dynamic segmentation | Boşalan connection en büyük kalan aralığı böler; küçük aralıkları bölmez; connection reuse yapar | Başta N eşit parça oluşturmak aynı davranış değildir |
| Resume | Offset'leri kaydeder, pause/crash sonrası devam eder | Engine sidecar yeterli değil; uygulama job kayıtlarını da saklamalı |
| Refresh download address | Kaynak sayfaya dönüp yeni linki bekler, mevcut download ile eşleştirir | Süresi dolan URL için kullanıcıya yeniden başlat demek yerine job yenilenmeli |
| Scheduler | Ayrı queue, start/stop, sıra değiştirme, concurrency, retry ve quota | Hazar'ın tek zaman penceresi bunun bir alt kümesi |
| Browser integration | Extension download/media yakalar; yerel analizde WS + native messaging fallback bulunuyor | Transport fallback ve kurulum akışı tamamlanmalı |
| Medya kullanımı | Video paneli, subtitle yakalama; tek kullanımlık link için ayrı capture akışı | Tespit, bütün içeriği ve doğru track'leri indirmekle aynı şey değil |
| Günlük kullanım | Kategoriler, Download All, drag/drop, proxy/auth seçenekleri | Motor dışında ürün eksikleri var |

Kaynaklar: [dynamic segmentation](https://www.internetdownloadmanager.com/support/segmentation.html), [scheduler](https://www.internetdownloadmanager.com/support/idm-scheduler/idm_scheduler.html), [genel özellikler](https://www.internetdownloadmanager.com/features2.html), [link refresh](https://www.internetdownloadmanager.com/register/new_faq/sites2_3.html), [tek kullanımlık video linkleri](https://www.internetdownloadmanager.com/register/new_faq/video9.html), [subtitle yakalama](https://www.internetdownloadmanager.com/register/new_faq/video16.html).

IDM'nin reklamındaki hız katsayısı ölçüm sonucu olarak alınmamalı. HTTP version, server connection limiti ve ağ kapasitesi sonucu değiştirir. Yerel rapordaki driver/DLL envanteri, encrypted HTTPS body'lerinin kernel driver tarafından tek başına okunabildiğini kanıtlamaz.

## 2. Hazar'da zaten bulunanlar

- HTTP Range download, 8 varsayılan/16 maksimum worker, connection pool.
- HEAD + Range probe, range yoksa single connection fallback.
- ETag/size metadata, diskteki part boyundan resume, retry/backoff, opsiyonel SHA-256.
- HLS master seçimi, AES-128, init segment, BYTERANGE ve segment resume.
- MV3 download/webRequest/DOM/page hook capture, cookie/Referer/header aktarımı.
- Manifest body capture ve browser frame'inde segment fetch fallback.
- Queue concurrency, günlük scheduler penceresi, HTTP file başına speed limit.
- Tray menüsü, updater, macOS/Windows release pipeline.
- DASH parser/download modülü; desktop akışına henüz bağlanmamış.

README ve PLAN bazı özellikleri hâlâ yok gösteriyor. Özellikle scheduler/speed limit/tray “tamamen eksik” değildir. Bunların kapsamı aşağıda ayrıca belirtilmiştir.

## 3. Öncelikli bug ve güvenilirlik açıkları

### P0 — Range response doğrulaması

Kanıt: `crates/hazar-engine/src/download.rs::try_part` yalnız status 206'yı kabul ediyor. Response `Content-Range` başlangıcı, bitişi ve toplamı istenen aralıkla karşılaştırılmıyor. Aynı boyda yanlış aralık dönerse son boy kontrolü bunu yakalamaz; SHA-256 opsiyoneldir.

Öneri: Her 206 için aralık/total doğrula. Yeni parçalarda da resource validator kullan; yalnız `written > 0` için If-Range göndermek, download sırasında dosya değişmesini tam korumaz. Strong ETag tercih et; weak ETag'i If-Range validator olarak kullanma. Last-Modified fallback politikasını açık belirle. Validator yoksa güven düzeyini belirt.

Done: yanlış offset, yanlış total ve download ortasında değişen resource hiçbir zaman başarılı dosya üretmemeli.

### P0 — Browser bridge kimlik ve input doğrulaması

Kanıt: `crates/hazar-localapi/src/server.rs` handshake subprotocol kontrol ediyor ama Origin allowlist kontrol etmiyor. Session eksikse mesaj reddedilmiyor (`session().is_some()` koşulu). Token time/client id + xorshift ile üretiliyor; pairing credential değil. Broadcast bütün client'lara gidiyor. `src-tauri/src/bridge.rs::on_bytes` stream_id'yi path içine doğrudan koyuyor.

Etki: Subprotocol bilen bir web sayfasının local bridge ile konuşması mümkün olabilir. Client'lar job bilgilerini paylaşabilir. Path separator içeren stream_id için path containment garantisi yok. Bunlar exploit ile doğrulanmadı; kontroller kodda eksik.

Öneri: Native host üzerinden pairing veya kullanıcıya görünür tek seferlik pairing; extension Origin allowlist; CSPRNG token; bütün mutating mesajlarda zorunlu session; job-owner kontrolü; job'a özel response routing; server tarafından üretilen disk id; payload/chunk/queue/disk limitleri. URL/token/Cookie log redaction ortak katmanda yapılmalı: engine status hatası URL'yi query dahil taşıyor, bridge hata metnini log'a yazıyor.

Done: Yetkisiz origin, eksik token, başka client'ın job'u, traversal stream_id ve limit üstü chunk reddedilmeli.

### P0 — Browser byte aktarımı tamamlanma garantisi

Kanıt: `extension/src/background.js::tunnelSegments` segmentleri tekrar fetch edip base64 JSON olarak gönderiyor. `send()` sonucu disk yazım ACK'i değil. `bridge.rs::on_bytes` son index gelince assemble ediyor; kalıcı receipt bitmap/hash yok. Duplicate chunk progress'i yeniden artırır. Senkron file I/O async message loop içinde çalışıyor.

Öneri: Job açma → indexed chunk → durable ACK → explicit finalize. Duplicate chunk idempotent olmalı. Reconnect sonrası eksik index'ler istenmeli. Byte queue bounded olmalı; ACK window/backpressure kullanılmalı. Büyük body'ler stream edilmeli; binary WS aktarımı pairing sonrası değerlendirilebilir. Native messaging'i control plane olarak kullanmak büyük medya body'lerini oraya taşımayı gerektirmez.

Done: Duplicate/out-of-order/reconnect/disk-full koşullarında doğru dosya veya açık failure; erken “done” olmamalı.

### P1 — Single connection yolu dosyayı erken yayımlıyor

Kanıt: `download.rs::single_stream` doğrudan destination üzerinde `File::create` yapıyor. Kesilince final isim altında yarım dosya kalır; mevcut dosya truncate olabilir. Part worker retry mekanizması bu yolda yok.

Öneri: `.partial` staging + size/hash kontrolü + final publish. Range olmayan kaynağa resume vaat etme; retry sıfırdan başlar. Overwrite politikasını kullanıcı seçimiyle yönet. Segmented assemble'da da hash doğrulamasını final rename'den önce yap.

Done: Kesilen/bozuk download mevcut başarılı dosyayı değiştirmemeli ve final isim altında başarılı görünmemeli.

### P1 — Queue/settings yalnız memory'de

Kanıt: `bridge.rs::start` boş Queue/VecDeque oluşturuyor; `lib.rs` her açılışta `Settings::default()` kullanıyor. Settings setter yalnız memory'yi değiştiriyor. QueueEntry gerçek request context'inin tamamını saklamıyor. Engine sidecar disk üzerinde olsa da app restart'ta işleri geri kuramıyor.

Öneri: SQLite job/settings store. Cookie/Authorization verilerini düz DB'ye yazma; OS credential store veya browser'dan refresh. Açılışta `running → interrupted`; kullanıcı resume veya uygun queue policy ile devam. Engine sidecar byte state için korunmalı.

Done: Restart sonrası queue sırası, settings, destination ve iş durumu korunmalı; resume eldeki part'ları kullanmalı.

### P1 — DASH var, desktop'ta çalışmıyor

Kanıt: `crates/hazar-engine/src/dash.rs::download_dash` mevcut; `bridge.rs::run_grab` içindeki Dash branch hâlâ sabit error döndürüyor. Bu bir integration eksiği.

Parser kapsamı da sınırlı: ilk Period içinden tek en yüksek bandwidth Representation seçiliyor. Audio/video ayrı seçilmiyor. SegmentTemplate inheritance yok. Timeline için sadece S element sayısı kullanılıyor; `t/d/r` işlenmiyor. `$Time$` gerçek timestamp yerine segment number ile değiştiriliyor. SegmentList Initialization child ve byte range'leri genel olarak modellenmiyor. Modül açıklamasındaki SegmentBase iddiası tam range/SIDX desteğine karşılık gelmiyor.

Öneri: Önce typed MPD parser + explicit destek kapsamı; desteklenmeyen shape'i temiz ret. Sonra track plan → segment download → mux → desktop wiring. Mevcut parser'ı bağlamak tek başına DASH desteğini tamamlamaz.

### P1 — Browser manifest yolu HLS bilgisini kaybediyor

Kanıt: `handleManifestBody` yalnız `#EXTM3U` kontrol ediyor; plaintext MPD'yi encrypted olarak işaretleyebilir. Bridge HLS request'te captured URL listesi varsa `plan_from_segments` kullanıyor. Yalın URL listesi key/IV/BYTERANGE/init/sequence bilgilerini taşımaz.

Etki: Normal engine manifest parser'ındaki AES-128 ve range desteği browser fallback yolunda korunmayabilir. Segment tespiti başarılı görünürken output yanlış veya eksik olabilir.

Öneri: `CapturedManifest { kind, final_url, body, frame_context }` veya typed segment plan gönder. HLS ve DASH ayrı parse edilmeli. Reklam/kalite/audio grupları tab başına en uzun URL listesi seçilerek birleştirilmemeli.

## 4. IDM seviyesine yaklaşmak için feature eksikleri

| Eksik | Mevcut durum | Önerilen davranış |
|---|---|---|
| Dynamic segmentation | `plan_parts` başta sabit parça oluşturuyor, worker bitince çıkıyor | Boş worker büyük kalan aralığı alır/böler; minimum boy ve host bütçesi |
| Pause/resume/retry UI | Engine cancel + sidecar var; UI ağırlıkla cancel | Job state machine; pause ayrı state, resume aynı job/destination |
| Link refresh | Extension recapture route var; mevcut job ile sağlam eşleştirme yok | `needs_refresh`; kaynak sayfa/asset/validator ile yeni request context bağlama |
| Medya mux | Raw segment concatenate | Video+audio track seçimi, doğru container, ffprobe doğrulama |
| HLS alternate audio/subtitles | EXT-X-MEDIA modellenmiyor | Dil/default track ve subtitle seçimi |
| Live recording | HLS plan bir snapshot; ENDLIST/target_duration takip döngüsü yok; dynamic DASH ret | Açık recording mode, playlist reload, sequence dedup, stop/finalize |
| Global speed/host limit | HTTP download başına limiter; HLS/DASH'ta yok | App toplam rate budget + host başına request/connection budget |
| Retry-After | Exponential backoff var; response Retry-After taşınmıyor | Delta/date Retry-After, jitter, host cooldown ve cancellation-aware bekleme |
| Scheduler stop | Pencere dışında yeni job başlamıyor; çalışanlar sürüyor | UI'da start-only veya start/stop policy seçimi |
| Queue yönetimi | Tek memory queue | Queue adı, sıra/öncelik, kategori/path rules; quota sonra |
| POST download | Body varlığı kaydediliyor; non-GET devralınmıyor | Method/body/content-type envelope; tekrar gönderim riski olan POST'ta explicit policy |
| Native fallback | Yalnız WS | Küçük Rust stdio host + installer registration |
| Browser dağıtımı | Unpacked extension | Chrome/Edge/Firefox store ve eşleşen protocol version |
| Safari | Extension manifest'i Safari dağıtımı değil | Kullanım ihtiyacı doğrulanınca Safari Web Extension paketi |
| Download All | Toplu dosya linki seçme flow'u yok | Sayfa linklerini filtreleyip preview → queue |
| Proxy/auth | Environment proxy ve header aktarımı | Per-host proxy/config; Basic UI; NTLM/Kerberos talebe göre |
| Release doğrulaması | Workflow yalnız engine testlerini gate ediyor | Bridge/extension/UI ve gerçek medya container kontrolünü release gate'e ekle |

Tray icon mevcut. Window kapanınca devam davranışı için ilgili close handler bu incelenen `lib.rs` içinde görünmüyor; runtime doğrulanmadan “tamam” sayılmamalı. Updater imzası macOS Developer ID/notarization ve Windows code signing yerine geçmez.

## 5. Teknoloji kararları

### Rust/Tauri/Svelte korunmalı

Temel stack eksiklerin kaynağı değil. Reqwest connection pooling zaten var. Connection reuse'u yeniden sıfırdan yazmaya gerek yok. Önce correctness, job persistence ve media pipeline tamamlanmalı.

### FFmpeg + ffprobe sidecar: öneriliyor

Segment download Hazar'da kalsın. FFmpeg local track'leri `-c copy` ile mux/remux etsin; ffprobe track/duration/container kontrolü yapsın. Bu yöntem re-encode gerektirmez; container/codec uyumsuzluğunda uygun MKV veya açık hata seçilmeli. Binary sürümü/checksum/build kapsamı sabitlenmeli; paket boyutu ve seçilen build'in lisansı dağıtımda değerlendirilmelidir. [FFmpeg streamcopy](https://ffmpeg.org/ffmpeg.html#Streamcopy)

### Typed DASH parser: öneriliyor

`dash-mpd` parser'ını mevcut downloader'ın önüne koymak uygun aday. Kütüphanenin downloader'ı deneysel olarak belgeleniyor; doğrudan bütün motoru değiştirmek yerine MPD → Hazar track plan adapter daha kontrollü. İlk kapsam static VOD, separate audio/video, template inheritance, timeline t/d/r, range'ler. [dash-mpd](https://docs.rs/dash-mpd/latest/dash_mpd/)

### SQLite: öneriliyor

Job/settings/history için SQLite ve küçük bir migration katmanı yeterli. Byte state engine sidecar'da kalır. Frontend localStorage kalıcı download state'in otoritesi olmamalı. Secret persistence ayrı tutulmalı.

### Native messaging: öneriliyor

WS'nin yanında control transport fallback ve pairing bootstrap olarak kullan. Installer host manifest'ini register etsin; allowed_origins store extension ID'leriyle sınırlı olsun. JSON mesajlarının browser limitleri nedeniyle medya aktarımı bounded/chunked olmalı. Native host launch ile Hazar GUI auto-start davranışı ayrı bir ürün kararıdır. [Chrome native messaging](https://developer.chrome.com/docs/extensions/develop/concepts/native-messaging)

### yt-dlp: opsiyonel extractor adapter

YouTube/site-specific URL çözümleme için imza/n cipher kodunu kendimiz sürdürmek yerine yt-dlp JSON output adapter değerlendirilebilir. Çözülen format/header/subtitle metadata Hazar'a aktarılır; transfer ve queue bizim motorda kalır. `403` refresh extractor üzerinden yapılabilir, başarı garantisi değildir. Güncel YouTube kapsamı yt-dlp-ejs + desteklenen JS runtime gerektiriyor; yalnız binary eklemek yeterli varsayılmamalı. Sürüm, update ve dependency paketi birlikte yönetilmeli. [yt-dlp resmi repo](https://github.com/yt-dlp/yt-dlp#dependencies)

### Browser response reuse: ikinci aşama

Şu anki frame fetch, player'ın tükettiği aynı segmenti yeniden request ediyor. Gerçek tek kullanımlık segment URL'lerinde bu da başarısız olabilir. Explicit capture mode içinde player response clone/stream tee ile ilk body'yi alma değerlendirilmeli. Memory/backpressure/cross-frame sınırları çözülmeden bütün response body'lerini toplama. CDP browser debugging izinleri nedeniyle varsayılan kullanıcı akışı için ağır; opt-in debug/capture aracı olarak kalabilir.

### HTTP/3, TLS impersonation, kernel driver: ilk aşamada gerek yok

HTTP/2 request sayısı TCP connection sayısıyla aynı değildir. Protocol ve host davranışı ölçülmeden “daha fazla connection” veya HTTP/3 hız garantisi vermez. TLS fingerprint sorunu gerçek failure örnekleriyle kanıtlanırsa alternatif client araştırılır. Kernel driver macOS/Windows geliştirme ve dağıtım maliyetini artırır; mevcut browser capture açıklarını tek başına çözmez. FTP/RTMP/site spider mevcut hedefte daha düşük önceliklidir.

## 6. Uygulama sırası ve kabul kriterleri

[1] Correctness: Content-Range/validator, staging publish, bridge auth/path/input, ACK/finalize.

Kabul: Yanlış aralık reddedilir; eski dosya korunur; yetkisiz bridge message reddedilir; duplicate/reconnect yanlış done üretmez.

[2] Kalıcı job yönetimi: SQLite, settings, pause/resume/retry, refresh context.

Kabul: App restart sonrası işler geri gelir; aynı URL/destination ile part resume çalışır; yeni signed URL doğrulanarak mevcut job'a bağlanır. URL değişti diye partial dosyalar otomatik silinmez; resource değiştiyse byte'lar karıştırılmaz.

[3] Medya: captured manifest envelope, typed DASH, HLS audio/subtitle, FFmpeg/ffprobe, desktop wiring.

Kabul: Ayrı audio/video içeren static MPD ve alternate audio HLS oynatılabilir dosya üretir. Timeline/inheritance/key/range fixture'ları doğru çözülür. HLS/DASH SHA-256 ve cancel bağlanır.

[4] Performans: bounded range work queue; önce küçük sabit chunk'larla worker reuse, sonra gerekiyorsa aktif kalan aralık split.

Kabul: Bir worker yavaşlayınca diğerleri boşta beklemez; gap/overlap yok; resume metadata aralıkları doğrular; host bütçesi aşılmaz. Başlangıç chunk/minimum split boyu benchmark sonucuyla ayarlanır.

[5] Dağıtım ve kullanım: native host, store extension, signing/notarization, kategori/Download All; talebe göre yt-dlp ve live recording.

Kabul: Temiz makinede extension/app version uyumu, WS failure fallback ve indirilen gerçek audio/video output doğrulanır.

## 7. Sonraki doğrulama planı — henüz çalıştırılmadı

- HTTP: yanlış Content-Range; weak/changed ETag; no-range dropped body; existing destination; 429 Retry-After; disk full.
- Job: restart; queued/running pause; scheduler stop; host concurrency; refresh URL ve cookie context.
- HLS: AES-128 browser capture; init/range metadata; audio rendition; discontinuity; live snapshot'in full video olarak işaretlenmemesi.
- DASH: audio/video AdaptationSet; inherited template; t/d/r ve r=-1; multi-Period; byte range; plaintext MPD capture.
- Transport: duplicate/out-of-order; reconnect; bounded buffered bytes; source tab kapanması; service worker restart; malicious path.
- Medya: yalnız hash değil; ffprobe track sayısı, duration ve playable container kontrolü.
- Performans: aynı endpoint/dosya/ağ ile 1/4/8/16 worker; HTTP/1.1 ve HTTP/2 ayrı; asimetrik segment hızı; disk kullanım tepe noktası; peak memory.
- IDM davranış doğrulaması: lisanslı Windows kurulumunda aynı kontrollü server'a karşı Range request zaman çizelgesi, pause/restart ve refresh akışı. HTTPS MITM yerine kontrol edilen server request log'u yeterli olabilir. Kurulum ve uygulama çalıştırma ayrıca kullanıcı tarafından başlatılmalı.

## Kaynaklar

- [IDM dynamic segmentation](https://www.internetdownloadmanager.com/support/segmentation.html)
- [IDM scheduler](https://www.internetdownloadmanager.com/support/idm-scheduler/idm_scheduler.html)
- [IDM features](https://www.internetdownloadmanager.com/features2.html)
- [IDM address refresh](https://www.internetdownloadmanager.com/register/new_faq/sites2_3.html)
- [IDM single-use video](https://www.internetdownloadmanager.com/register/new_faq/video9.html)
- [IDM subtitles](https://www.internetdownloadmanager.com/register/new_faq/video16.html)
- Yerel referans: `~/Dev/re/idm/ANALYSIS.md`, `~/Dev/re/idm/EXTENSION.md`; eski rapordaki implementation notları güncel Hazar durumunu göstermiyor.
