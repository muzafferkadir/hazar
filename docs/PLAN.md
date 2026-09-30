# Hazar — plan

Referans: `~/Dev/re/idm/ANALYSIS.md` (IDM 6.43 build 11 analizi).
Kural: clean-room. IDM'nin kodu/asset'i/protokol stringi kopyalanmaz; sadece davranış referans.

## Ürün

macOS + Windows için çok connection'lı indirme yöneticisi. Ürün adı **Hazar**,
site `hazar.mkdir.dev`. Arayüz + update + deploy hattı klyppr-desktop'tan alındı,
sıfırdan yazılan tek şey motor.

## Mimari

```
crates/hazar-engine   probe → plan → worker'lar → assemble → hash    (UI'sız, test edilebilir)
crates/hazar-cli      `hazar get|probe` — motoru UI'sız çalıştırma yolu
src-tauri             Tauri v2 kabuğu: IPC komutları + progress event'leri
src                   Svelte 5 UI (şu an: tek indirme + canlı progress + log)
<dest>.hazar/         meta.json + part-NNN.bin (kesintiye dayanıklı resume state)
```

- Local API / browser köprüsü (M2): aynı engine'in üstüne `localapi` modülü
  (127.0.0.1 loopback WS + HTTP JSON). Extension buradan konuşacak.
- Kernel sniffing yok. Yakalama tamamen extension tarafında (MV3
  `downloads.onDeterminingFilename` + `declarativeNetRequest` + content script).

## Motor kararları (uygulandı)

| Konu | Karar |
|---|---|
| Probe | HEAD → gerekirse `GET Range: bytes=0-0` (CDN'ler HEAD'de `Accept-Ranges` vermiyor) |
| Plan | varsayılan 8 connection, cap 16, part başına min 1 MiB |
| Resume | `.hazar/` sidecar: `meta.json` + `part-NNN.bin`; ETag/size değişirse baştan |
| Devam | part bazlı `Range` + `If-Range`; kesilen body diskte kalır, retry kaldığı yerden |
| Retry | exponential backoff (probe dahil), 429/5xx/timeout/truncated body retryable |
| Fallback | range yok / server yok saydı → tek connection |
| Bitiş | part'lar birleşir → dosya boyu doğrulanır → opsiyonel SHA-256 → sidecar silinir |

## Milestone'lar

| # | İçerik | Durum |
|---|---|---|
| M0 | Motor + CLI: probe/plan/8 connection/assemble/sha256 | **bitti** |
| M1 | Resume + retry + fallback + ETag koruması | **bitti** |
| M2 | `localapi` (loopback WS, subprotocol + session) + MV3 extension (Chrome/Edge/Firefox) | **bitti** |
| M3 | Cookie geçişi (extension `cookies` API) + Referer/UA/header aktarımı | **bitti** (store yayını hariç) |
| M4 | HLS (m3u8 + AES-128 + resume + header) | **bitti** — DASH tespit edilir, indirme yok |
| M5 | UI: queue (var), scheduler, speed limit, tray/menubar, kategori | kısmi |
| M6 | Windows installer + notarize edilmiş macOS DMG + auto-update | **bitti** (v0.1.1 release: universal DMG + NSIS/MSI + updater artifact'ları) |
| Ek | Extension dağıtımı | **store yok** — release'e `artifacts/hazar-extension-vX.zip` eklenir, kullanıcı "load unpacked" ile yükler |

### M2/M3 — yakalama mimarisi (uygulandı)

```
extension (MV3)
  downloads.onCreated ─┐
  webRequest (5 tip)  ─┼─► grab (named JSON) ─► localapi (127.0.0.1:8722..8730, ws hazar.v1)
  content + page-hook ─┘                              │
  recapture.html ◄── declarativeNetRequest rule ──┐   ▼
                                                  └── bridge (src-tauri) ─► hazar-engine
```

- Protokol: isimli JSON (`hello`/`grab`/`cancel`/`media`/`ping` ↔ `hello_ok`/`grab_ack`/`progress`/
  `finished`/`failed`/`queue`/`pong`). IDM'in positional opcode array'i **kullanılmadı**.
- Kimlik: subprotocol zorunlu + ilk mesaj `hello` + bağlantıya özel session token.
- Tarayıcı indirmesi devralındıktan sonra app ack vermezse `downloads.resume` ile geri veriliyor.
- POST/PUT indirmeleri bilinçli olarak devredilmiyor (engine body replay etmiyor).
- HLS sniff: `.m3u8` görülünce playlist bir kez fetch edilir, segment listesi app'e birlikte gider;
  MSE/blob streamleri için `page-hook.js` XHR/fetch'i izleyip segment URL'lerini grupluyor.

M2 notu: IDM'nin yolu `ws://127.0.0.1:1001` + subprotocol; biz kendi mesaj şemamızı
yazacağız (opcode array değil, isimli JSON event'ler).

## Test stratejisi

- `cargo test --workspace`: kontrollü mock HTTP server (range desteği yok / yok sayan /
  body'si kesilen / ETag değişen / 429) + plan ve hash unit testleri.
- Gerçek ağ smoke: `hazar probe` + `hazar get` (farklı `-n` değerleriyle hash karşılaştırma).
- CI: tag'de `cargo test -p hazar-engine` → sonra paketleme.

## Açık konular

- Windows tarafı packaging + imzalama (v1 ad-hoc/imzasız, istenirse EV cert).
- macOS: dağıtım için Developer ID + notarization gerekir (şu an ad-hoc imza).
- İsim/marka: `hazar` npm/crates/pypi boş, github `hazar` dolu → org `hazardm` kullanılacak.
- Extension store yayını: ilk sürüm unpacked/dev, sonra Chrome Web Store + AMO.
