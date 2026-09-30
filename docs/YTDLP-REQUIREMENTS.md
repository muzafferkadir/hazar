# yt-dlp gereksinimleri ve Hazar eksikleri

Araştırma: 2026-10-01. Mevcut kod: Hazar 0.2.4. Bu rapor code review ve resmi docs karşılaştırmasıdır; canlı site doğrulaması yapılmadı.

## 0.2.5 uygulama durumu

- Diagnostic mesajları wire protocol'de taşınır; popup hata nedeni ve tekrar analiz gösterir. Rate limit için host cooldown vardır.
- Özel header'lar bundled Hazar context plugin üzerinden origin ile sınırlıdır. Redirect koruması urllib/requests/curl_cffi yollarını kapsar; canlı redirect doğrulaması yapılmadı.
- Download başlamadan WS üzerinden fresh context istenir. Tab/page değişmişse işlem durur; secret context queue serialization'dan çıkarılır.
- Chrome 132+ selected-frame partition cookie sorgusu eklenmiştir. Netscape jar gerçek browser izolasyonunun tamamını yeniden oluşturmaz; aynı isim/path'te seçili partition değeri job'a özel önceliklidir.
- Fixed proxy seçim/basit bypass, explicit proxy/source address ve host bazlı impersonation vardır. PAC ve auto-detect otomatik çözülmez; çoklu route/bypass veya browser proxy auth challenge için garanti yoktur.
- bgutil 2.0.0 Deno script provider, dependencies ve macOS arm64/x64 / Windows x64 canvas addon'ları paketlenir. HTTP provider yoktur. Guest/login retry en fazla iki attempt'tir.
- Doğrulama: engine/localapi/app build ve JS/Python syntax; tool metadata ile Deno provider version 2.0.0 ve curl_cffi impersonation target listesi. Canlı token mint/download/session/redirect/PAC kontrolü yapılmadı.

### Dependency update politikası

Sürümler/checksum'lar `scripts/package-youtube.mjs` ve `scripts/package-pot.mjs` içinde sabittir. Provider kaynak/plugin sürümü birlikte değiştirilir; npm lockfile dependency integrity'lerini belirler. Paketleme sırasında yeni native addon sürümü/hash'leri ve lisanslar birlikte güncellenir. Update uygulamanın yeni release'iyle dağıtılır; user config veya rastgele plugin dizini yüklenmez. Gelecek update'te canlı site kontrolleri ayrıca istenmelidir.

## Araştırma anındaki 0.2.4 durumu

| Gereksinim | Hazar durumu | Eksik / karar |
| --- | --- | --- |
| FFmpeg | Paketli, mux/remux ve track kontrolü var | Yeni bağımlılık gerekmiyor |
| JavaScript runtime + EJS | Deno 2.9.7 ve resmi standalone yt-dlp içindeki EJS var | Güncellenirken yt-dlp/EJS birlikte ele alınmalı |
| Browser cookie jar | Domain/path/expiry/secure/HttpOnly ve tab cookie store aktarılıyor | Partitioned cookie sorgusu yok; cookie refresh yalnızca probe/download başında |
| Referer / User-Agent | Process argümanlarıyla aktarılıyor | Gerçek request UA override'ı her sayfa probe'unda bulunmayabilir |
| Özel HTTP header | GrabRequest headers taşıyabiliyor | yt-dlp BrowserContext ve process bu header'ları kullanmıyor |
| Proxy / IP | yt-dlp için açık bir proxy/source-address aktarımı yok | Browser-only proxy/PAC ile aynı çıkış garanti değil |
| TLS impersonation | Resmi binary lisans listesinde curl_cffi var | Kullanılabilir target'lar doğrulanmadı; site bazlı politika yok |
| YouTube PO token | Provider/token aktarımı yok | Deno/EJS bunu sağlamaz; plugin dirs şu an kapalı |
| Login/session refresh | Mevcut browser cookie'leri alınır | YouTube account cookie rotation ve restart sonrası refresh akışı eksik |
| Rate limit | Aynı anda iki probe, kısa cache var | Site bazlı cooldown/backoff yok; video açıkken periyodik probe tekrarlanabilir |
| Hata teşhisi | Download stderr son ERROR korunur | Probe status/error atılıyor; app `.ok().flatten()` ile hatayı boş adaya çeviriyor |
| Extractor güncelliği | Sabit sürüm 2026.08.19, checksum ile paketlenmiş | Ayrı dependency update politikası yok |

## Resmi docs bulguları

### Genel session ve network

Bazı servislerde video request'lerinin aynı IP, cookie ve HTTP header bilgileriyle yapılması gerekir. Browser ile app aynı bilgisayarda olsa bile browser-only proxy veya farklı IPv4/IPv6 route bu eşleşmeyi bozabilir. Cloudflare için güncel browser cookie ve tam User-Agent gereksinimi belgelenmiş. Bu koşulları sağlamak her 403'ü çözme garantisi değildir.

Cookie jar için Netscape formatı uygundur. Windows satır sonu uyumluluğu kontrol edilmeli; şu an writer tüm platformlarda LF üretir. Aynı process içinde yt-dlp'nin aldığı Set-Cookie jar'ına yazılabilir, fakat Hazar bunu browser'a geri senkronlamaz.

Kaynak: https://github.com/yt-dlp/yt-dlp/wiki/FAQ

### JavaScript challenge ve PO token ayrı gereksinimler

YouTube challenge çözümlemesi için runtime ve EJS gerekir. Deno önerilen runtime'dır. Resmi standalone binary EJS içerir; Hazar'ın paketleme tercihi bu gereksinimi karşılar.

Kaynak: https://github.com/yt-dlp/yt-dlp/wiki/EJS

PO token farklı bir attestation bilgisidir. Kullanılan client'a göre video server/player/subtitle request'lerinde gerekebilir. Token client, session veya video ID bağlamına bağlıdır. Browser request'inden alınan tek token'ın farklı video/client için kullanılabileceği varsayılmamalı. Resmi guide manuel token kopyalamak yerine provider öneriyor.

Kaynak: https://github.com/yt-dlp/yt-dlp/wiki/PO-Token-Guide

Provider seçenekleri:

- bgutil: Deno/Node ile HTTP service veya çağrı başına generation script; ayrıca yt-dlp provider plugin gerekir. Hazar için mevcut Deno ile çağrı başına script seçeneği araştırılabilir. Paketleme/bağımlılık ve eşzamanlı kullanım ayrıca değerlendirilmelidir.
- wpc: deneysel; nodriver ve Chrome/Chromium kullanır, çalışırken ayrı browser açar. Mevcut extension'dan otomatik token aktarımı sağlayan hazır bir çözüm değildir.

Kaynaklar: https://github.com/Brainicism/bgutil-ytdlp-pot-provider ve https://github.com/coletdjnz/yt-dlp-getpot-wpc

### Login ve browser context

YouTube account cookie'leri açık sekmelerde yenilenebilir. Login gerektirmeyen içerik için account cookie kullanmak zorunlu değildir. Önerim: YouTube public içerikte guest session; gerekli olduğunda account session. Diğer sitelerin session ihtiyacı site bazlı ele alınmalıdır.

Kaynak: https://github.com/yt-dlp/yt-dlp/wiki/Extractors

Partitioned cookie'ler top-level site bağlamına bağlıdır. Chrome API varsayılan olarak unpartitioned cookie okur; ilgili frame partitionKey açıkça istenmelidir. Netscape jar bu izolasyonu temsil etmediğinden bütün partition'ları birleştirmek doğru olmaz. Yalnızca seçilmiş tab/frame context'ini ayrı job'a aktarma veya browser fetch kullanma araştırılmalıdır.

Kaynak: https://developer.chrome.com/docs/extensions/reference/api/cookies

### TLS fingerprint

User-Agent browser network fingerprint'inin tamamı değildir. Bazı siteler TLS impersonation ister. yt-dlp curl_cffi desteği sunar. Hazar binary'sinin ilgili dependency lisansı var; target kullanılabilirliği ve gerçek site davranışı henüz doğrulanmadı.

Kaynak: https://github.com/yt-dlp/yt-dlp#impersonation

## Uygulama sırası önerisi

1. Probe hata bilgisini kaybetme: unsupported, login required, 403, 429, PO token/runtime, timeout ve dependency failure ayrımı. Secret içermeyen tanı bilgisi ve UI açıklaması. Aynı hata için otomatik tekrar döngüsü kurma.
2. Request context: doğru tab/frame ve cookie store; güncel session; gerekli özel header'ları hedef origin ile sınırla. Authorization/Cookie'yi global `--add-headers` ile farklı host'lara dağıtma.
3. Network context: explicit proxy/source-address seçenekleri ve browser-only proxy farkının teşhisi; site bazlı impersonation ve backoff. Browser proxy ayarlarını sessizce değiştirme.
4. YouTube PO token: mevcut Deno'dan yararlanan sabit sürüm/checksum ile paketli provider değerlendirmesi. Kontrollü plugin path, client/session/video ID bağlama ve expiry. Deno/EJS token provider olarak sunulmamalı.
5. yt-dlp/EJS/provider update politikası ve kullanıcı isterse canlı kontrol matrisi: public/login YouTube, Vimeo, Instagram, X, iframe/Cloudflare, proxy, 403/429. Sadece başarı değil output video/audio ve hata nedeni de doğrulanmalı.

Normal Hazar adayları ve kırmızı yt-dlp adayları ayrı kalır. Araştırma sonrası 0.2.5 kodu hazırlandı; mevcut doğrulama sınırları yukarıda belirtilmiştir.
