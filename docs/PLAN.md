# Hazar — mevcut kapsam

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

Tek queue ve HTTP/HTTPS GET yeterli. DRM, live recording, multi-Period DASH, POST replay, site spider, kernel driver, HTTP/3, TLS impersonation ve ayrı site extractor motoru eklenmedi. Native messaging/store/signing ek kurulum ve dağıtım işi olarak kalır.

Detaylı ilk inceleme: `IDM-GAP-ANALYSIS.md`. Oradaki tespitler v0.2.0 öncesi working tree'ye aittir.
