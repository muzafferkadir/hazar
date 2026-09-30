// Pinned standalone FFmpeg, checksum verified before bundling.
import { createHash } from 'node:crypto'
import { mkdir, writeFile, chmod } from 'node:fs/promises'
import { execFileSync } from 'node:child_process'
const base = 'https://github.com/eugeneware/ffmpeg-static/releases/download/b6.1.1/'
const hashes = {
 'ffmpeg-darwin-arm64':'a90e3db6a3fd35f6074b013f948b1aa45b31c6375489d39e572bea3f18336584',
 'ffmpeg-darwin-x64':'ebdddc936f61e14049a2d4b549a412b8a40deeff6540e58a9f2a2da9e6b18894',
 'ffmpeg-win32-x64':'04e1307997530f9cf2fe35cba2ca7e8875ca91da02f89d6c7243df819c94ad00',
 'darwin-arm64.LICENSE':'cb48bf09a11f5fb576cddb0431c8f5ed0a60157a9ec942adffc13907cbe083f2',
 'darwin-x64.LICENSE':'2e1d16c72fd74e12063776371da757322f8b77589386532f4fd8634bde7de1af',
 'win32-x64.LICENSE':'8ceb4b9ee5adedde47b31e975c1d90c73ad27b6b165a1dcd80c7c545eb65b903',
}
const dir = new URL('../src-tauri/media/', import.meta.url)
await mkdir(dir, { recursive: true })
async function download(name, target = name) {
 const response = await fetch(base + name)
 if (!response.ok) throw new Error(`${name}: ${response.status}`)
 const body = Buffer.from(await response.arrayBuffer())
 if (createHash('sha256').update(body).digest('hex') !== hashes[name]) throw new Error(`checksum: ${name}`)
 await writeFile(new URL(target, dir), body)
 return new URL(target, dir)
}
if (process.platform === 'darwin') {
 const arm = await download('ffmpeg-darwin-arm64')
 const x64 = await download('ffmpeg-darwin-x64')
 await download('darwin-arm64.LICENSE'); await download('darwin-x64.LICENSE')
 execFileSync('lipo', ['-create', arm.pathname, x64.pathname, '-output', new URL('ffmpeg', dir).pathname])
 await chmod(new URL('ffmpeg', dir), 0o755)
 execFileSync('codesign', ['--force', '--sign', '-', new URL('ffmpeg', dir).pathname])
 const { unlink } = await import('node:fs/promises'); await unlink(arm); await unlink(x64)
} else if (process.platform === 'win32') {
 await download('ffmpeg-win32-x64', 'ffmpeg.exe'); await download('win32-x64.LICENSE')
} else { throw new Error('Media packaging supports macOS and Windows') }
await writeFile(new URL('SOURCES.txt', dir), 'FFmpeg 6.1.1 standalone executable (separate process).\nBuilds and corresponding sources: https://github.com/eugeneware/ffmpeg-static/releases/tag/b6.1.1\nUpstream source: https://ffmpeg.org/releases/ffmpeg-6.1.1.tar.xz\nBuild license is included alongside the executable. Hazar code remains MIT.\n')

await import("./package-youtube.mjs")
