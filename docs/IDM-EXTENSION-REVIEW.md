# IDM extension → Hazar 0.2.2

2026-10-01. İnceleme: yerel IDM MV3 extension kaynakları (`~/Dev/re/idm/payload_x/IDMGCExt59.crx.unz.patched/{background,content,document}.js`), `~/Dev/re/idm/EXTENSION.md`, Hazar extension/localapi/desktop bridge. IDM kodu Hazar'a kopyalanmadı; davranışlar Hazar'ın kendi protokolü ve UI'ı ile uygulandı.

Bu statik kod incelemesidir. Lisanslı Windows IDM runtime karşılaştırması veya yeni Hazar build'inde canlı audio/video download doğrulaması yapılmadı. Gerçek eski Hazar output'u ffprobe ile incelendi: yalnız H.264 video, 2575.041667 saniye, 1311454160 byte; audio stream yok.

## Video panelinin mekanizması

IDM content script'i media elementlerini kaydeder. `Za` source URL, aktif element ve en büyük görünür video üzerinden player seçer; `D` player geometrisini ve görünürlüğünü hesaplar; `gb` app'e element/başlık/frame/rect bilgisini iletir. `ResizeObserver`, `IntersectionObserver`, DOM style observer, scroll ve resize listener'ları konumu günceller. Panel bu element/frame bilgisini kullanır. Yalnız ağda video URL'i bulmak paneli doğru player'a bağlamak için yeterli değildir.

Hazar'da panel content script içinde, kendi Shadow DOM'u ile oluşturulur. Ağ adayları aynı frame'e filtrelenir. Doğrudan video source'u eşleşirse o URL kullanılır; blob source'lu birden fazla player için yalnız tek oynayan player'a aday bağlanır. Belirsiz player eşleştirmesinde buton gizlenir. HLS master variant'ları kalite menüsü oluşturur; seçilen variant'ın AUDIO grubu çözülür. Scroll, resize, style/class/source değişimi, DOM'dan kaldırılma, fullscreen ve bfcache dönüşü takip edilir. Buton player kontrol event'lerine yayılmaz. URL/token menü metnine yazılmaz.

## Karşılaştırma

| IDM mekanizması | Hazar 0.2.2 durumu | Kanıt / kalan fark |
|---|---|---|
| Download devralma | Var | `downloads.onCreated`; app ACK öncesi pause, ACK sonrası cancel/erase; ACK yoksa resume |
| MIME/Content-Disposition/URL ile ad/tip | Var | `lib.js`, request header/response metadata ve `buildRequest` |
| Request/header/cookie capture | Var | webRequest + cookies API; secret header'lar disk state'e yazılmaz |
| DOM media tespiti | Var | video/audio/source/embed/object/meta; MutationObserver |
| XHR/fetch page-context capture | Var | Manifest text/arraybuffer/blob gövdesi; redirect sonrası gerçek response URL |
| Player üstü download paneli | Eklendi | `content.js`: frame scope, görünürlük, position, Shadow DOM |
| Kalite seçimi | HLS için var | Master variants; yüksek bandwidth ilk seçenek. DASH kalite seçimi UI'ı yok |
| Ayrı HLS audio | Browser yolu düzeltildi | AUDIO group/default/autoselect seçimi; ayrı track capture; mux bitmeden done yok |
| HLS encryption/init/range | Browser capture planı eklendi | AES-128/IV/key rotation URI, MAP, BYTERANGE; desteklenmeyen encryption açık hata |
| Subtitle download | Kısmi | DOM `<track>` subtitle/caption URL'leri panelde; HLS/DASH subtitle rendition planı yok |
| Download All / selection | Eklendi | Sağ tık; bulunduğu frame'deki dosya linkleri, dedup, 200 limit. IDM'nin filtreli çok frame/site taraması değil |
| Context menu media/blob | Düzeltildi | Blob/page URL'i dosya diye gönderilmez; aynı frame'deki yakalanmış stream seçilir |
| Yeniden yakalama | Var | DNR recapture akışı; queue link refresh |
| Ana transport / reconnect | Var | Token/session/owner kontrolü olan loopback WS; port taraması, keepalive, reconnect |
| Native messaging fallback | Eksik | Stdio host + OS/browser registration gerekir; WS'ye başka wrapper eklemek gerçek transport fallback değildir |
| Custom protocol ile app açma | Eksik | App kapalıyken browser download korunur; otomatik app başlatılmaz |
| Force/bypass modifier keys | Eksik | IDM content script'i modifier key state'i app'e aktarır; Hazar'da site/capture ayarları ve explicit gönderme var |
| App'ten browser'a Set-Cookie | Eksik | Browser cookies okunur; app response cookie'leri browser'a geri yazılmaz |
| POST/body replay | Eksik | Hazar HTTP/HTTPS GET; non-GET browser'da bırakılır |
| Proxy/auth context transfer | Kısmi | Cookie/Referer/headers var; browser PAC/proxy ve auth challenge aktarımı yok |
| Site-specific player adapters | Eksik | Genel DOM/network/MSE tespiti; IDM'deki site kuralları paketlenmedi |
| Live recording | Eksik | ENDLIST olmayan HLS full video diye kaydedilmez |
| Store kurulumu / native installer | Eksik | Chrome/Edge/Firefox unpacked dağıtım |

## Ses bug'ının nedeni ve fix

Önceki resolver master'ı seçilen video media playlist'ine indirgerken EXT-X-MEDIA audio bilgisini düşürüyordu. Browser fallback yalnız video segmentlerini birleştiriyordu. Native engine'in master audio desteği bu yola uygulanmıyordu.

Artık audio rendition seçilen variant'ın AUDIO group'una göre çözülür. Browser video ve audio capture planlarını ayrı tutar; init/range/key bilgisi korunur. App `audio_start` sınırına göre iki track dosyası oluşturur. FFmpeg `-map 0:v:0 -map 1:a:0 -c copy` ile mux yapar. Mux başarısızsa done gönderilmez, track parçaları korunur. Chunk failure app'e bildirilir. Eski sessiz output'a otomatik ses eklenmez; source oturumu ile yeniden download gerekir.

## Açık kalan işler

Tam IDM eşitliği sağlanmış değildir. Native transport/installer, POST/proxy/auth, modifier keys, HLS/DASH subtitle planı, live capture ve site-specific adapters ayrı işlerdir. Bunların varmış gibi gösterilmesi veya Windows IDM runtime doğrulaması olmadan parity iddiası yapılması doğru olmaz.

Panel davranışı için resmi kaynak: https://www.internetdownloadmanager.com/register/new_faq/video2.html

Tek kullanımlık link / force key davranışı: https://www.internetdownloadmanager.com/register/new_faq/video9.html
