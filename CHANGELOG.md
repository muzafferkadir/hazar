# Changelog

## 0.3.0 — 2026-10-01

- Uygulama adı **Hazar Download Manager**; göl mavisi yeni logo, app + extension için ortak renk token'ları, light/dark mode.
- Ayarlara dil seçeneği (English varsayılan, Türkçe). Dil app'ten extension'a otomatik geçer; dil dosyaları `src/locales` ve `extension/src/locales`.
- Video paneli: sağ üstte küçük logo, tıklayınca kalite listesi (2160p…144p), × ile kapanır; ↻ ile manuel yeniden tarama (popup'ta da).
- yt-dlp tüm kaliteleri listeler, seçilen yükseklik indirilir. Analiz metadata'sı download'da tekrar kullanılır; aynı video URL varyantları için tekrar analiz yapılmaz.
- yt-dlp adayları hata kırmızısı yerine outline stil; bağlantı yokken doğru hata mesajı.

## 0.2.5 — 2026-10-01

- yt-dlp probe hataları login/PO token/403/429/runtime/unsupported/DRM/timeout olarak sınıflanır ve popup'ta gösterilir; rate limit cooldown ve manuel tekrar analiz.
- Download başlangıcında extension'dan fresh browser context; tab/page değişmişse açık ret. Seçilen frame partition cookie'leri job'a özel jar'a aktarılır.
- Captured özel header'lar origin ile sınırlı bundled plugin üzerinden uygulanır; cross-origin redirect'te auth bilgileri temizlenir. YouTube kendi auth/header üretimini kullanır.
- Browser fixed proxy seçimi, manuel proxy/source address ve site bazlı Chrome impersonation. PAC/auto-detect için manuel proxy gereksinimi açık gösterilir.
- Sabit sürüm/checksum ile paketli bgutil 2.0.0 PO token Deno script provider; HTTP service veya ayrı Node kurulumu gerekmez. macOS canvas addon'ları arm64/x64, Windows x64.
- YouTube önce guest session kullanır; login gerektiğinde mevcut browser account session ile bir kez retry eder.
- Cookie/header/token context kalıcı queue state'e yazılmaz. Timeout/cancel process ağacını sonlandırır; provider cache job'ın geçici klasöründe kalır.

## 0.2.4 — 2026-10-01

- yt-dlp, YouTube dışındaki video sayfalarını da metadata probe ile çözebilir; download kullanıcı seçimiyle başlar.
- Hazar medya adayları korunur; yt-dlp adayları popup ve video panelinde kırmızı renk + yt-dlp etiketiyle ayrı gösterilir.
- Browser oturumundan domain/path/secure/expiry/HttpOnly bilgili cookie jar, Referer ve User-Agent yt-dlp'ye aktarılır. Cookie jar işlem sonunda silinir; cookie değerleri queue state'e yazılmaz.
- yt-dlp probe cache ve iki process sınırı; playlist/live/DRM adayları gösterilmez. Normal download için otomatik fallback eklenmez.

## 0.2.3 — 2026-10-01

- YouTube watch/embed/shorts/live/youtu.be linkleri normal HTML file download yerine yt-dlp yoluna yönlenir.
- Sabit sürüm/checksum ile paketlenen yt-dlp + Deno; EJS solver standalone binary içinde, FFmpeg audio/video mux.
- Video/audio track kontrolünden önce done verilmez; extractor hatasında HTML fallback yoktur.
- YouTube popup/player paneli raw fragment yerine gerçek video linkini gönderir.
- HTML response file download için açık hata üretir; eski HTML YouTube queue kayıtları startup’ta failed olur.

## 0.2.2 — 2026-10-01

- Browser HLS akışında seçilen variant’ın ayrı audio rendition bilgisi korunur; video/audio capture sonrası FFmpeg mux yapılır. AES-128/init/BYTERANGE metadata capture planında tutulur.
- Header sabit ve butonlar dışındaki alanlardan sürüklenebilir; URL formu butonla açılır.
- Queue kaydı silme ve listeyi sıfırlama; indirilen dosyalar korunur.
- Video üstü download paneli, HLS kalite seçimi, frame/görünürlük/scroll/resize/fullscreen takibi ve DOM subtitle linkleri.
- Sağ tıkla sayfadaki/seçili download linklerini gönderme.
- Popup, video paneli ve context menu aynı HLS çözümleme akışını kullanır.
- Browser download kaydı app ACK vermeden silinmez.

## 0.2.1 — 2026-09-30

- Master variant URL’lerinin segment sayılması düzeltildi; browser akışı tam VOD playlist çözer.
- Aynı PHP endpoint’indeki farklı query URL’leri birbirini ezmez.
- Cross-origin segment fetch’i player’ın cookie/CORS davranışını izler.
- Playlist/HTML response’ları media chunk olarak kaydedilmez; eski yanlış tamamlanan HLS kayıtları hata olarak gösterilir.
- Browser fallback aynı queue kaydında ve doğru player frame’inde devam eder.


## 0.2.0 — 2026-09-30

- UI: tek link formu ve download listesi; pause/resume, link yenileme ve update kontrolü.
- Queue ve settings restart sonrası korunur. Cookie/Authorization disk state'e yazılmaz.
- Boşalan worker kalan aralıkları indirir; aynı anda açık request sayısı sınırlıdır.
- HTTP Content-Range/If-Range kontrolü, güvenli staging output ve no-range retry.
- Desktop static DASH desteği: XML inheritance/timeline ve ayrı audio/video.
- Captured HLS metadata korunur; alternate audio, checksum doğrulanan FFmpeg ile mux edilir.
- Browser byte aktarımı disk ACK bekler; duplicate chunk ve değişen capture planı kontrol edilir.
- Bridge web origin'lerini, eksik session ve başka client'a ait job mesajlarını reddeder.
- Eski single-download IPC, ayrı progress/log ekranı ve kullanılmayan CSS kaldırıldı.
- Mor gradient tarzında yeni download logosu ve app icon’ları.
- Klyppr’ın native macOS transparency ve vibrancy efekti geri eklendi.
- macOS universal ve Windows release'e FFmpeg/lisans/source referansları eklendi; release kontrolleri genişletildi.
