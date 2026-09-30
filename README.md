# Hazar

macOS ve Windows için sade download manager. Tauri 2 + Svelte 5 arayüz, Rust engine ve Chrome/Edge/Firefox extension.

## Kullanım

1. App'e HTTP/HTTPS linkini yapıştır ve **İndir** seç.
2. Download listesinde **Duraklat**, **Devam et** veya **Link yenile** kullan.
3. **Ayarlar** üzerinden klasörü, eşzamanlı download sayısını ve browser capture'ı değiştir.
4. Eklentiyi **Eklentiyi kaydet** ile dışarı çıkar; Chrome/Edge'de Extensions → Developer mode → Load unpacked ile yükle.

Window kapatılınca app tray/menu bar'da çalışmaya devam eder. Tray'deki Çıkış app'i kapatır. Update kontrolü Ayarlar içindedir.

## Destek

- HTTP(S): bounded range work queue, connection pool, If-Range, Content-Range kontrolü, retry, sidecar resume ve opsiyonel SHA-256.
- Worker tamamlanan aralığın ardından sıradaki aralığı alır; aktif HTTP aralıkları download sırasında kesilmez.
- HLS VOD: captured manifest body, master seçimi, AES-128, init segment, BYTERANGE ve ayrı audio track.
- Static DASH: typed XML, template inheritance, Number/Time templates, timeline, SegmentList ve separate audio/video.
- Separate tracks, paketlenen FFmpeg ile re-encode olmadan mux edilir. Kaynak/lisans bilgileri app resource'larında bulunur.
- Browser capture: downloads/webRequest/DOM/page hook; cookie/Referer/header aktarımı; frame fetch fallback için disk ACK.
- Queue/settings app data klasöründe `downloads.json` içine kaydedilir. Restart sonrası yarım işler otomatik başlamaz; Devam et ile başlatılır.
- Cookie/Authorization disk state'e yazılmaz. Restart sonrası oturum isteyen kaynak tarayıcıdan yeniden gönderilmelidir.

## Sınırlar

- DRM, YouTube signature/cipher, POST/PUT replay, live recording ve multi-Period DASH yok.
- HLS URL listesi fallback'i manifest metadata'sı yoksa AES/range bilgilerini yeniden oluşturamaz. Manifest body tercih edilir.
- Browser frame fetch URL'yi tekrar request eder; gerçek tek kullanımlık segment URL'leri başarı garantisi taşımaz.
- App kapalıyken download yok. Native messaging ve extension store yayını yok; extension unpacked dağıtılır.
- macOS paket ad-hoc imzalıdır; Developer ID/notarization ve Windows code signing bu release'in kapsamında değildir.
- UI tek queue kullanır. Ek kategori, site spider, protocol sürücüsü veya extractor motoru yok.

## Geliştirme

```bash
pnpm install
cargo test --workspace
pnpm check && pnpm build
pnpm check:extension && pnpm test:extension
cargo run -p hazar-testmatrix
```

Medya regression testi FFmpeg ister. Release binary'sini hazırlamak için `node scripts/package-media.mjs` kullanılır. Uygulamayı kullanıcı başlatır: `pnpm tauri dev`.

## Release

```bash
bash scripts/bump-version.sh 0.2.0
node scripts/package-media.mjs
bash scripts/package-extension.sh
# Değişiklikleri commit/push yaptıktan sonra:
git tag v0.2.0
git push origin v0.2.0
```

Tag workflow macOS universal DMG ve Windows installer/update artifact'larını üretir. Engine/localapi/media testleri ve frontend/extension kontrolleri packaging öncesi çalışır. Updater signing key GitHub Secrets'tadır; repoya yazılmaz.

IDM karşılaştırması: [docs/IDM-GAP-ANALYSIS.md](docs/IDM-GAP-ANALYSIS.md).
