# Changelog

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
