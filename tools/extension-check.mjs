/**
 * Kurulu Hazar.app içindeki eklentiyi headless Chrome'a yükleyip gerçek app'e
 * bağlandığını doğrular (CDP üzerinden service worker durumu okunur).
 *
 *   node /tmp/hz-ext-check.mjs
 */
import { spawn } from 'node:child_process';

const CHROME =
  process.env.HAZAR_CHROME ||
  `${process.env.HOME}/Library/Caches/ms-playwright/chromium-1234/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing`;
const EXTENSION = process.env.HAZAR_EXTENSION_DIR || '/Applications/Hazar.app/Contents/Resources/extension';
const PORT = 9333;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const child = spawn(
  CHROME,
  [
    '--headless=new',
    `--remote-debugging-port=${PORT}`,
    '--user-data-dir=/tmp/hz-ext-check-profile',
    `--load-extension=${EXTENSION}`,
    `--disable-extensions-except=${EXTENSION}`,
    '--no-first-run',
    '--disable-gpu',
    'about:blank',
  ],
  { stdio: 'ignore' },
);

class Cdp {
  constructor(socket) {
    this.socket = socket;
    this.id = 0;
    this.waiters = new Map();
    socket.addEventListener('message', (event) => {
      const message = JSON.parse(event.data);
      const waiter = this.waiters.get(message.id);
      if (waiter) {
        this.waiters.delete(message.id);
        waiter(message);
      }
    });
  }
  call(method, params = {}, session) {
    const id = ++this.id;
    const payload = { id, method, params };
    if (session) payload.sessionId = session;
    this.socket.send(JSON.stringify(payload));
    return new Promise((resolve) => this.waiters.set(id, resolve));
  }
}

try {
  let wsUrl = null;
  for (let i = 0; i < 40 && !wsUrl; i++) {
    try {
      const response = await fetch(`http://127.0.0.1:${PORT}/json/version`);
      wsUrl = (await response.json()).webSocketDebuggerUrl;
    } catch {
      await sleep(250);
    }
  }
  if (!wsUrl) throw new Error('chrome devtools çalışmadı');

  const socket = new WebSocket(wsUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve);
    socket.addEventListener('error', () => reject(new Error('cdp bağlanamadı')));
  });
  const cdp = new Cdp(socket);

  // service worker hedefini bekle
  let worker = null;
  for (let i = 0; i < 40 && !worker; i++) {
    const targets = await cdp.call('Target.getTargets');
    worker = (targets.result?.targetInfos ?? []).find(
      (target) => target.type === 'service_worker' && target.url.endsWith('src/background.js'),
    );
    if (!worker) {
      // SW'yi uyandır: bir sayfa aç
      await cdp.call('Target.createTarget', { url: 'about:blank' });
      await sleep(500);
    }
  }
  if (!worker) throw new Error('eklenti service worker bulunamadı (yüklendi mi?)');
  console.log('eklenti yüklendi:', worker.url);

  const attached = await cdp.call('Target.attachToTarget', { targetId: worker.targetId, flatten: true });
  const session = attached.result.sessionId;

  // bağlanması için biraz bekle
  let state = null;
  for (let i = 0; i < 20; i++) {
    const evaluated = await cdp.call(
      'Runtime.evaluate',
      {
        expression:
          "JSON.stringify(typeof __hazarDebug === 'function' ? { connected: __hazarDebug().connected, port: __hazarDebug().port, settings: __hazarDebug().settings } : { error: 'debug hook yok' })",
        returnByValue: true,
      },
      session,
    );
    state = JSON.parse(evaluated.result?.result?.value ?? '{}');
    if (state.connected) break;
    await sleep(500);
  }

  console.log('eklenti durumu:', JSON.stringify(state));
  if (state.connected) {
    console.log(`✅ kurulu app'teki eklenti gerçek uygulamaya bağlandı (port ${state.port})`);
  } else {
    console.log('❌ bağlanmadı — app çalışıyor mu? (127.0.0.1:8722)');
    process.exitCode = 1;
  }
} finally {
  child.kill('SIGKILL');
}
