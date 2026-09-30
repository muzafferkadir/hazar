// Official standalone extractor + JS challenge runtime. Versions/checksums are pinned.
import { createHash } from 'node:crypto'
import { mkdir, writeFile, chmod, mkdtemp, readFile, rm } from 'node:fs/promises'
import { execFileSync } from 'node:child_process'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
const dir = new URL('../src-tauri/media/', import.meta.url)
await mkdir(dir, { recursive: true })
const assets = {
 'yt-dlp_macos': ['yt-dlp/yt-dlp', '2026.08.19', '0f192b7ec147ab6288885d6351d9ab67367640029b4377576ef46dd79cf7b202'],
 'yt-dlp.exe': ['yt-dlp/yt-dlp', '2026.08.19', '66674953fe251b89f4d08c5f0e35e0728679bd67ab3d7d05c0562af101dd3e7a'],
 'deno-aarch64-apple-darwin.zip': ['denoland/deno', 'v2.9.7', '5cd46d6268f6f78f5d88bdc7159d20bd44cdaa4b3303474839f87ec6fe7ae25c'],
 'deno-x86_64-apple-darwin.zip': ['denoland/deno', 'v2.9.7', '95daaff11c116a52ad54785e7914c8e9c9cdcaba793c5ed929c74ca2d8e6259a'],
 'deno-x86_64-pc-windows-msvc.zip': ['denoland/deno', 'v2.9.7', 'a0c3101b4158d1dfb7d6a78a7bf0f3de80c96bb423c152beec8beb22786f2238'],
}
const work = await mkdtemp(join(tmpdir(), 'hazar-youtube-package-'))
async function fetchAsset(name) {
 const [repo, tag, hash] = assets[name]
 const response = await fetch(`https://github.com/${repo}/releases/download/${tag}/${name}`)
 if (!response.ok) throw new Error(`${name}: ${response.status}`)
 const body = Buffer.from(await response.arrayBuffer())
 if (createHash('sha256').update(body).digest('hex') !== hash) throw new Error(`checksum: ${name}`)
 const file = join(work, name); await writeFile(file, body); return file
}
async function unpack(name, target, binary) {
 const archive = await fetchAsset(name); await mkdir(target)
 if (process.platform === 'win32') execFileSync('powershell', ['-NoProfile', '-Command', `Expand-Archive -LiteralPath '${archive.replaceAll("'", "''")}' -DestinationPath '${target.replaceAll("'", "''")}'`])
 else execFileSync('unzip', ['-q', archive, '-d', target])
 return join(target, binary)
}
try {
 if (process.platform === 'darwin') {
  const [yt, arm, x64] = await Promise.all([fetchAsset('yt-dlp_macos'), unpack('deno-aarch64-apple-darwin.zip', join(work, 'arm'), 'deno'), unpack('deno-x86_64-apple-darwin.zip', join(work, 'x64'), 'deno')])
  await writeFile(new URL('yt-dlp', dir), await readFile(yt))
  execFileSync('lipo', ['-create', arm, x64, '-output', new URL('deno', dir).pathname])
  for (const name of ['yt-dlp', 'deno']) {
   await chmod(new URL(name, dir), 0o755)
   execFileSync('codesign', ['--force', '--sign', '-', new URL(name, dir).pathname])
  }
 } else if (process.platform === 'win32') {
  const [yt, deno] = await Promise.all([fetchAsset('yt-dlp.exe'), unpack('deno-x86_64-pc-windows-msvc.zip', join(work, 'windows'), 'deno.exe')])
  await writeFile(new URL('yt-dlp.exe', dir), await readFile(yt)); await writeFile(new URL('deno.exe', dir), await readFile(deno))
 } else throw new Error('YouTube packaging supports macOS and Windows')
 for (const [name, repo, tag, path] of [['YT-DLP', 'yt-dlp/yt-dlp', '2026.08.19', 'LICENSE'], ['YT-DLP-THIRD-PARTY', 'yt-dlp/yt-dlp', '2026.08.19', 'THIRD_PARTY_LICENSES.txt'], ['DENO', 'denoland/deno', 'v2.9.7', 'LICENSE.md']]) {
  const response = await fetch(`https://raw.githubusercontent.com/${repo}/${tag}/${path}`)
  if (!response.ok) throw new Error(`license ${name}: ${response.status}`)
  await writeFile(new URL(`${name}.LICENSE`, dir), await response.text())
 }
 await writeFile(new URL('YOUTUBE-SOURCES.txt', dir), 'yt-dlp 2026.08.19 official standalone (bundles yt-dlp-ejs): https://github.com/yt-dlp/yt-dlp/releases/tag/2026.08.19\nDeno 2.9.7 official runtime: https://github.com/denoland/deno/releases/tag/v2.9.7\nPinned release asset SHA-256 checksums: scripts/package-youtube.mjs\nSeparate processes; no user browser database access, remote components, or user configuration. Bundled plugins only; POT sources: pot/SOURCES.txt.\n')
 await import('./package-pot.mjs')
 console.log('YouTube dependencies packaged')
} finally { await rm(work, { recursive: true, force: true }) }
