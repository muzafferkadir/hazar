# Hazar

Multi-connection download manager for macOS and Windows: segmented HTTP(S)
downloads, resume, HLS (m3u8) streams, and browser capture via a companion
extension.

- Site: `https://hazar.mkdir.dev`
- Engine: `crates/hazar-engine` (Rust, no UI dependencies)
- CLI: `crates/hazar-cli` → `hazar get|hls|probe <url>`
- App: `src-tauri` (Tauri v2) + `src` (Svelte 5)
- Extension: `extension/` (MV3, Chrome/Edge/Firefox)
- Bridge: `crates/hazar-localapi` (loopback WebSocket)

## Layout

```
crates/hazar-engine/    probe, plan, workers, assemble, hash, hls
crates/hazar-cli/       `hazar` command line front end
crates/hazar-localapi/  loopback WS API + wire protocol types
src-tauri/              Tauri v2 shell: IPC commands, progress events, capture bridge
src/                    Svelte UI (manual download + captured queue + log)
extension/              MV3 browser extension (see extension/README.md)
<dest>.hazar/           sidecar state: meta.json + part-NNN.bin (resume)
```

## Engine

```rust
use hazar_engine::{DownloadOptions, Downloader};

let opts = DownloadOptions::new(url, dest)
    .connections(8)
    .header("Referer", "https://site.example/watch")
    .header("Cookie", "sid=abc");
let outcome = Downloader::new(opts)?.run().await?;
```

- `probe` — HEAD, then `GET Range: bytes=0-0` to confirm ranges (CDNs often
  advertise no `Accept-Ranges` on HEAD).
- `plan` — at most 8 parts by default (cap 16), never below 1 MiB per part.
- workers — one ranged request per part, `If-Range` guarded resume, retry with
  backoff, partial bodies stay on disk and continue from the recorded offset.
- `assemble` — parts are concatenated into the target file, the optional SHA-256
  is verified, then `<dest>.hazar/` is removed.

### HLS

```rust
use hazar_engine::{download_hls, HlsOptions};

let outcome = download_hls(HlsOptions {
    manifest: "https://cdn.example/vod/master.m3u8".into(),
    segments: None,   // sniffed segment list, when the playlist was not captured
    base_url: None,
    output: "/tmp/movie.ts".into(),
    connections: 8,
    user_agent: None,
    headers: vec![("Referer".into(), "https://site.example/watch".into())],
    expected_sha256: None,
    cancel: None,
}, None).await?;
```

Master playlist → highest `BANDWIDTH` variant, `EXT-X-KEY` AES-128 (CBC/PKCS7),
`EXT-X-MAP` init segment, `EXT-X-BYTERANGE`, per-segment resume (`.partial` →
rename), assembled into one file.

## CLI

```bash
cargo run -p hazar-cli -- probe https://example.com/file.iso
cargo run -p hazar-cli -- get  https://example.com/file.iso -o file.iso -n 8 --referer https://site/watch --cookie "sid=abc"
cargo run -p hazar-cli -- hls  https://cdn.example/vod/master.m3u8 -o movie.ts -n 8
```

`Ctrl-C` stops the run and keeps the state; rerun the same command to resume.

## Desktop app

```bash
pnpm install
pnpm tauri dev
```

The app starts the capture bridge on `127.0.0.1:8722` (falls through to 8730) and
shows extension captures next to the manual download form.

Releases are cut from tags (`git tag v0.1.2 && git push origin v0.1.2`) by
`.github/workflows/release.yml`; the updater reads
`https://github.com/muzafferkadir/hazar/releases/latest/download/latest.json`.

Before tagging, bump the version in **both** `package.json` and
`src-tauri/tauri.conf.json` so the tag and the app version match — the updater
compares the installed version against `latest.json`'s `version` field.
Signing secrets: `TAURI_SIGNING_PRIVATE_KEY` + `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
(private key also in Bitwarden Secrets as `HAZAR_TAURI_SIGNING_PRIVATE_KEY`).

## Browser extension

`extension/` hands downloads to the app over the loopback WebSocket and sniffs
HLS/DASH. **No store listing**: every release ships an unpacked zip
(`artifacts/hazar-extension-vX.Y.Z.zip` → `bash scripts/package-extension.sh`), and the
user loads the extracted folder with "load unpacked". Capture paths, the wire
protocol and the step-by-step install guide live in `extension/README.md`.

## Capture test matrix

`crates/hazar-testmatrix` scores how well hard sites are handled. Every row is a
deterministic local scenario: the resolver must find the stream, the engine must
download it, and the bytes must match.

```bash
cargo run -p hazar-testmatrix                  # 31 offline + 6 live-layer self-test rows
cargo run -p hazar-testmatrix -- --browser     # + headless Chrome, extension, browser-transport rows
cargo run -p hazar-testmatrix -- --coverage    # fixture × strategy matrix + score table
HAZAR_LIVE=1 cargo run -p hazar-testmatrix -- --live   # + real site profiles (detection only)
```

- 31 offline fixtures: files (ranges / no ranges / redirects / cookie and referer
  gates / 429 and 503+Retry-After / dropped connection), HLS (master variants,
  AES-128, `EXT-X-MAP` + `EXT-X-BYTERANGE`, expiring tokens, live playlist,
  truncated segment, SAMPLE-AES refusal), DASH (SegmentTemplate, Widevine), page
  shapes (og:video, JSON-LD, jwplayer/videojs/DPlayer/pkplyr configs, iframe),
  obfuscation (eval-packed JS, base64, hex escapes), segment-only MSE streams,
  multi-source picking, dead-mirror fallback and **ad/preroll skipping**
  (ad candidates are dropped in every resolver path, and Chromium blocks ad
  hosts while a live row runs).
- Browser layer: real headless Chromium (Chrome for Testing) loads `extension/`,
  completes the loopback handshake, intercepts a download and hands it over.
- Live layer: detection **plus a first-segment fetch** (first 64 KiB, `Range`) —
  never a full download. Profiles carry player type, required referer/cookie/iframe
  and whether the site needs the browser transport (manifest built at runtime).
  `HAZAR_LIVE_<SITE>_URL` points a site at a real media page and `HAZAR_PROXY`
  routes both the engine and Chromium through a proxy (loopback stays direct).
  Failed rows print their own classification (`bot wall`, `navigation failed`,
  `404`, `no player on the page`). The layer's own logic is verified every run by
  `live-selftest-*` rows (same profiles, same assertions, local site-shaped
  pages, including the gated path). DRM and signature/cipher rows are gated by
  design (`docs/CAPTURE-TESTPLAN.md`).

Exit code is non-zero on a red row, which is what the goal loop repeats on.

Definition of done (offline + browser matrix green, youtube gated, player-gated
sites verified through capture mode, legal/headless ceilings documented) lives in
`docs/CAPTURE-TESTPLAN.md`.

## End-to-end (kurulu uygulama)

Reproduce the release smoke test against the installed app:

```bash
open -a Hazar                                    # köprü 127.0.0.1:8722'de dinlemeye başlar
node tools/app-e2e.mjs                           # hello → 2 eşzamanlı grab (biri hız limitli) → sha256 → iptal
node tools/extension-check.mjs                   # kurulu app'in içindeki eklentiyi headless Chrome'a yükler, bağlantıyı doğrular
node tools/app-capture-e2e.mjs                   # "Sayfada indir" bayt akışı: 3 parça → birleştirme + sha256
```

`app-e2e.mjs` gerçek bir dosyayı indirir (`/tmp/hz-e2e`) ve referans SHA-256 ile
karşılaştırır; iptal testinde yarım kalan `.hazar` sidecar'ının resume için durduğunu
kontrol eder. `extension-check.mjs` `HAZAR_EXTENSION_DIR` (varsayılan:
`/Applications/Hazar.app/Contents/Resources/extension`) ve `HAZAR_CHROME` ile
özelleştirilebilir.

## Tests

```bash
cargo test --workspace     # engine + localapi + matrix fixtures
pnpm check && pnpm build   # Svelte + TypeScript
pnpm test:extension        # extension helpers (node, no browser)
pnpm test:bridge           # real rust app + websocket, end to end
pnpm test:matrix           # the capture matrix
```

Engine tests run against a local HTTP server that can lie about range support,
truncate bodies, change ETags, or serve AES-128 encrypted HLS.

## Not done yet

- DASH (`.mpd`) download — detected and captured, but not assembled.
- Live-site probing is a separate, user-run layer (`HAZAR_LIVE=1`).
- POST/PUT downloads — recognised and deliberately left to the browser
  (the engine cannot replay request bodies yet).
- UI: scheduler, speed limit, tray/menu bar, categories.
- Store publishing (Chrome Web Store / AMO) and Developer ID notarization.
