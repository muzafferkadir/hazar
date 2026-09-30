# Hazar — mevcut kapsam

## v0.2.5

- Diagnostic/cooldown, fresh browser context, scoped headers, selected-frame partition cookies.
- Fixed/explicit proxy ve source IP, host bazlı impersonation.
- Paketli bgutil/Deno script PO token provider; YouTube guest/login bounded retry.

## v0.2.4

- Genel yt-dlp video probe ve kullanıcı seçimiyle alternatif download.
- Popup/player panelinde kırmızı yt-dlp adayları; browser cookie jar + Referer/User-Agent aktarımı.

## v0.2.3

- YouTube public video extractor: pinned yt-dlp/Deno/EJS, ayrı track mux ve output track kontrolü.
- HTML video diye tamamlanmaz; YouTube video/audio track kontrolü yapılır.

## v0.2.2

- Sabit/sürüklenebilir header; URL ekle ile açılan form; queue kaydı silme/listeyi sıfırlama.
- Frame bazlı video paneli, HLS kalite menüsü, DOM subtitle linkleri, sağ tık toplu/seçili dosya linkleri.
- Browser HLS ayrı audio capture ve mux; AES-128/MAP/BYTERANGE capture planı; chunk failure bildirimi.
- Browser download app ACK öncesi silinmez. Master playlist segment adayları yüzünden cache'den düşmez.
- IDM extension karşılaştırması ve açık farklar: `IDM-EXTENSION-REVIEW.md`.

## v0.2.0

- Tek download listesi; link ekle, pause/resume, link refresh, klasörde göster.
- Atomik dosya state'i ile queue/settings persistence; cookie/auth header'ları saklanmaz.
- Sabit aktif worker bütçesi; tamamlanan worker kalan küçük aralıkları alır. Aktif aralık split yapılmaz.
- HTTP range/validator kontrolü; staging output ve checksum sonrası final publish.
- Captured HLS body ile AES/range/init metadata korunur; alternate audio mux.
- Static DASH XML parser ve desktop download; template inheritance, timeline, separate audio/video.
- FFmpeg ayrı executable olarak paketlenir; checksum, lisans ve source referansları dahildir.
- Browser bridge origin/session kontrolü, job ownership, bounded message/chunk ve disk ACK.
- Restart sonrası yarım işler interrupted olur; kullanıcı Devam et seçer.
- Tray'de devam, updater ve macOS/Windows release workflow.

## Bilinçli sınırlar

Tek queue ve HTTP/HTTPS GET yeterli. DRM, live recording, multi-Period DASH, POST replay, site spider, kernel driver, HTTP/3, TLS impersonation kapsam dışıdır. yt-dlp alternatif extractor olarak YouTube dışında da kullanılabilir. Native messaging/store/signing ek kurulum ve dağıtım işi olarak kalır.

Detaylı ilk inceleme: `IDM-GAP-ANALYSIS.md`. Oradaki tespitler v0.2.0 öncesi working tree'ye aittir.
