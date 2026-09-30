/**
 * Gerçek uygulamaya karşı uçtan uca test (app çalışır durumda olmalı).
 *
 *   node /tmp/hz-e2e.mjs
 *
 * Yaptığı:
 *  1) köprüye hello → settings/feature doğrulaması
 *  2) hız limitli grab (10 MB @1.5 MB/s) + eşzamanlı ikinci grab (2 MB)
 *  3) grab_ack / progress / finished olayları, dosya + sha256 doğrulaması
 *  4) canlı bir indirmeyi iptal etme testi
 */
import { createHash } from 'node:crypto';
import fs from 'node:fs';

const APP = process.env.HAZAR_BRIDGE || 'ws://127.0.0.1:8722/hazar';
const DIR = '/tmp/hz-e2e';
const EXPECTED_10MB = 'e5b844cc57f57094ea4585e235f36c78c1cd222262bb89d53c94dcb4d6b3e55d';

fs.mkdirSync(DIR, { recursive: true });

const jobs = new Map(); // id -> {name, startedAt, finishedAt, lastPct}
let session = null;
let phase = 'hello';
const finished = new Map();

const ws = new WebSocket(APP, 'hazar.v1');
const startedAt = Date.now();
const stamp = () => `[${((Date.now() - startedAt) / 1000).toFixed(1)}s]`;

function grab(id, request) {
  jobs.set(id, { name: request.filename, startedAt: Date.now(), lastPct: -1 });
  ws.send(JSON.stringify({ type: 'grab', session, id, request }));
  console.log(`${stamp()} → grab ${id} ${request.url} (limit=${request.speed_limit_bps ?? '-'} B/s)`);
}

ws.addEventListener('open', () => {
  console.log(`${stamp()} bağlandı: ${APP}`);
  ws.send(JSON.stringify({ type: 'hello', protocol: 1, client: 'e2e-test', version: '0.1.2' }));
});
ws.addEventListener('error', (event) => console.error(`${stamp()} ws hata`, event.message ?? ''));

ws.addEventListener('message', (event) => {
  const message = JSON.parse(event.data);

  if (message.type === 'hello_ok') {
    session = message.session;
    const s = message.settings;
    console.log(`${stamp()} hello_ok: app ${message.app} ${message.version} · protocol ${message.protocol}`);
    console.log(
      `         settings: connections=${s.connections} max_concurrent=${s.max_concurrent_downloads} ` +
        `schedule=${s.schedule_enabled}(${s.schedule_from}-${s.schedule_to}) download_dir=${s.download_dir}`,
    );
    console.log(`         features: ${message.features.join(', ')}`);

    grab('e2e-limitsiz-10mb', {
      url: 'https://proof.ovh.net/files/10Mb.dat',
      kind: 'file',
      filename: 'limited-10mb.bin',
      save_dir: DIR,
      connections: 4,
      speed_limit_bps: 1_500_000,
    });
    grab('e2e-kucuk-2mb', {
      url: 'https://speed.cloudflare.com/__down?bytes=2000000',
      kind: 'file',
      filename: 'small-2mb.bin',
      save_dir: DIR,
      connections: 4,
    });
    return;
  }

  if (message.type === 'grab_ack') {
    console.log(`${stamp()} ack ${message.id} → ${message.state}`);
    return;
  }

  if (message.type === 'progress') {
    const total = message.total || 0;
    const pct = total ? Math.floor((message.written / total) * 100) : 0;
    const job = jobs.get(message.id);
    if (job && pct >= job.lastPct + 25) {
      job.lastPct = pct;
      console.log(`${stamp()} ${message.id}: ${pct}% (${message.phase})`);
    }
    return;
  }

  if (message.type === 'finished') {
    const job = jobs.get(message.id);
    const seconds = ((Date.now() - (job?.startedAt ?? startedAt)) / 1000).toFixed(1);
    console.log(`${stamp()} ✅ finished ${message.id} → ${message.path} (${message.size} B, ${seconds}s)`);
    finished.set(message.id, message);
    return;
  }

  if (message.type === 'failed') {
    console.log(`${stamp()} ❌ failed ${message.id}: ${message.reason}`);
    finished.set(message.id, { failed: message.reason });
  }
});

function sha256(path) {
  return createHash('sha256').update(fs.readFileSync(path)).digest('hex');
}

async function waitFor(predicate, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return true;
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error(`zaman aşımı: ${label}`);
}

try {
  await waitFor(() => session !== null, 15_000, 'hello_ok');
  await waitFor(() => finished.size >= 2, 90_000, 'iki indirmenin bitmesi');

  console.log('\n== dosya doğrulaması');
  const big = `${DIR}/limited-10mb.bin`;
  const small = `${DIR}/small-2mb.bin`;
  const bigHash = sha256(big);
  const smallHash = sha256(small);
  const bigStat = fs.statSync(big);
  console.log(`  limited-10mb.bin: ${bigStat.size} B · sha256 ${bigHash.slice(0, 16)}…`);
  console.log(`  small-2mb.bin:    ${fs.statSync(small).size} B · sha256 ${smallHash.slice(0, 16)}…`);
  console.log(`  hash referansla eşleşiyor mu: ${bigHash === EXPECTED_10MB ? '✅ evet' : '❌ hayır'}`);

  // İptal testi: büyük bir dosyayı başlat, 2 sn sonra iptal et.
  console.log('\n== iptal testi');
  grab('e2e-iptal', {
    url: 'https://proof.ovh.net/files/100Mb.dat',
    kind: 'file',
    filename: 'cancelled.bin',
    save_dir: DIR,
    connections: 4,
  });
  await new Promise((resolve) => setTimeout(resolve, 2500));
  ws.send(JSON.stringify({ type: 'cancel', session, id: 'e2e-iptal' }));
  console.log(`${stamp()} cancel gönderildi: e2e-iptal`);
  await new Promise((resolve) => setTimeout(resolve, 4000));
  const cancelled = finished.get('e2e-iptal');
  console.log(
    cancelled
      ? `  iptal sonucu: ${cancelled.failed ? 'failed → ' + cancelled.failed : 'finished (beklenmiyordu!)'}`
      : '  iptal edilen iş finished raporlamadı ✅',
  );
  const partial = `${DIR}/cancelled.bin.hazar`;
  console.log(`  yarım kalan sidecar duruyor mu (resume için): ${fs.existsSync(partial) ? '✅ evet' : 'hayır'}`);

  phase = 'done';
  console.log('\nSONUÇ: uçtan uca akış tamam');
} catch (error) {
  console.error(`\nHATA: ${error.message}`);
  process.exitCode = 1;
} finally {
  ws.close();
}
