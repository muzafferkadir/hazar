// Pinned bgutil Deno provider and platform canvas addons; npm lockfile pins JS dependencies.
import { createHash } from 'node:crypto'
import { mkdir, writeFile, readFile, cp, rm, mkdtemp, readdir } from 'node:fs/promises'
import { execFileSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
const root = fileURLToPath(new URL('../', import.meta.url))
const target = join(root, 'src-tauri/media/pot')
const archives = {
 source: ['https://codeload.github.com/Brainicism/bgutil-ytdlp-pot-provider/tar.gz/refs/tags/2.0.0', '47038f7e0556a3d689044460f1b6d78381212e1bd04723a27358e1a42279f866'],
 plugin: ['https://github.com/Brainicism/bgutil-ytdlp-pot-provider/releases/download/2.0.0/bgutil-ytdlp-pot-provider.zip', 'bce874dfa25896c2798e0f4f8147b7b22e785479eb1e459ab232bf2506c95016'],
 'darwin-arm64': ['https://github.com/Automattic/node-canvas/releases/download/v3.2.3/canvas-v3.2.3-napi-v7-darwin-arm64.tar.gz', '38c296c9d81c05598db849fb8103543d6649fec7246c35197d9be8199c75116f'],
 'darwin-x64': ['https://github.com/Automattic/node-canvas/releases/download/v3.2.3/canvas-v3.2.3-napi-v7-darwin-x64.tar.gz', '74a549f88cc570042951a77cb7baec4661d08b2d79ccff08352a5e8d4a7c86d9'],
 'win32-x64': ['https://github.com/Automattic/node-canvas/releases/download/v3.2.3/canvas-v3.2.3-napi-v7-win32-x64.tar.gz', 'ba953cc8c38303ab94cc83461c7561506a5a3af37d6c678de4a47914d2d0bb48'],
}
const work = await mkdtemp(join(tmpdir(), 'hazar-pot-package-'))
async function asset(key) {
 const [url, hash] = archives[key]
 const response = await fetch(url)
 if (!response.ok) throw new Error(`POT asset ${key}: ${response.status}`)
 const body = Buffer.from(await response.arrayBuffer())
 if (createHash('sha256').update(body).digest('hex') !== hash) throw new Error(`POT checksum ${key}`)
 const file = join(work, key === 'plugin' ? 'plugin.zip' : `${key}.tar.gz`)
 await writeFile(file, body); return file
}
try {
 await mkdir(target, { recursive: true })
 const source = join(work, 'source'); await mkdir(source)
 execFileSync('tar', ['-xzf', await asset('source'), '--strip-components=1', '-C', source])
 const server = join(source, 'server')
 execFileSync(process.platform === 'win32' ? 'npm.cmd' : 'npm', ['ci', '--omit=dev', '--ignore-scripts', '--no-audit', '--no-fund'], { cwd: server, stdio: 'inherit', shell: process.platform === 'win32' })
 await rm(join(server, 'node_modules/.bin'), { recursive: true, force: true })
 const canvas = join(server, 'node_modules/canvas/build/Release'); await mkdir(canvas, { recursive: true })
 const platforms = process.platform === 'darwin' ? ['darwin-arm64', 'darwin-x64'] : ['win32-x64']
 for (const platform of platforms) {
  const directory = join(work, platform); await mkdir(directory)
  execFileSync('tar', ['-xzf', await asset(platform), '-C', directory])
  await cp(join(directory, 'build/Release'), join(canvas, platform), { recursive: true, dereference: true })
  if (process.platform === 'darwin') {
   for (const name of await readdir(join(canvas, platform))) {
    if (name.endsWith('.node') || name.endsWith('.dylib')) execFileSync('codesign', ['--force', '--sign', '-', join(canvas, platform, name)])
   }
  }
 }
 const bindings = join(server, 'node_modules/canvas/lib/bindings.js')
 await writeFile(bindings, (await readFile(bindings, 'utf8')).replace("require('../build/Release/canvas.node')", "require(`../build/Release/${process.platform}-${process.arch}/canvas.node`)"))
 await rm(join(target, 'server'), { recursive: true, force: true })
 await mkdir(join(target, 'server'), { recursive: true })
 for (const name of ['src', 'node_modules', 'package.json', 'package-lock.json']) await cp(join(server, name), join(target, 'server', name), { recursive: true, dereference: true })
 const plugins = join(target, 'plugins'); await mkdir(plugins, { recursive: true })
 const payload = join(plugins, 'hazar'); await mkdir(payload, { recursive: true })
 const archive = await asset('plugin')
 if (process.platform === 'win32') execFileSync('powershell', ['-NoProfile', '-Command', `Expand-Archive -LiteralPath '${archive.replaceAll("'", "''")}' -DestinationPath '${payload.replaceAll("'", "''")}' -Force`])
 else execFileSync('unzip', ['-qo', archive, '-d', payload])
 await rm(join(payload, 'yt_dlp_plugins/extractor/getpot_bgutil_http.py'), { force: true })
 const provider = join(payload, 'yt_dlp_plugins/extractor/getpot_bgutil_script.py')
 let python = await readFile(provider, 'utf8')
 python = python.replace("'run', '--allow-env', '--allow-net',", "'run', '--no-config', '--node-modules-dir=manual', '--cached-only', '--allow-env', '--allow-net',")
 await writeFile(provider, python)
 await cp(join(root, 'scripts/ytdlp/yt_dlp_plugins'), join(payload, 'yt_dlp_plugins'), { recursive: true })
 await cp(join(source, 'LICENSE'), join(target, 'LICENSE'))
 await writeFile(join(target, 'SOURCES.txt'), 'bgutil-ytdlp-pot-provider 2.0.0 (GPL-3.0): https://github.com/Brainicism/bgutil-ytdlp-pot-provider/tree/2.0.0\ncanvas 3.2.3: https://github.com/Automattic/node-canvas/releases/tag/v3.2.3\nVersions, SHA-256 and packaging adaptations: scripts/package-pot.mjs\nJS dependency versions/integrities and bundled licenses: server/package-lock.json and server/node_modules\nOnly bundled plugin path is enabled. Provider scripts run on demand with bundled Deno.\n')
 console.log('PO token provider packaged')
} finally { await rm(work, { recursive: true, force: true }) }
