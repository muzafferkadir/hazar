/**
 * Bayt aktarımı (segment gövdeleri) uçtan uca testi — app çalışır durumda olmalı.
 *
 *   node tools/app-capture-e2e.mjs
 *
 * Ne yapar: köprüye hello → `bytes` mesajlarıyla 3 parçalı sahte bir segment
 * akışı gönderir → app parçaları birleştirip dosyayı yazmalı → sha256 doğrulanır.
 */
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const APP = process.env.HAZAR_BRIDGE || 'ws://127.0.0.1:8722/hazar';
const dir = '/tmp/hz-capture-e2e';
fs.mkdirSync(dir, { recursive: true });

const parts = [
  Buffer.from('HAZAR-CAPTURE-1|'.repeat(50)),
  Buffer.from('HAZAR-CAPTURE-2|'.repeat(120)),
  Buffer.from('HAZAR-CAPTURE-3|'.repeat(30)),
];
const expected = Buffer.concat(parts);
const expectedHash = createHash('sha256').update(expected).digest('hex');
const filename = 'capture-e2e.ts';

const ws = new WebSocket(APP, 'hazar.v1');
let session = null;
const messages = [];

const done = new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error('zaman aşımı (finished gelmedi)')), 30_000);
  ws.addEventListener('message', (event) => {
    const message = JSON.parse(event.data);
    messages.push(message.type);
    if (message.type === 'hello_ok') {
      session = message.session;
      parts.forEach((part, index) => {
        ws.send(
          JSON.stringify({
            type: 'bytes',
            session,
            stream_id: 'e2e-capture',
            index,
            total: parts.length,
            url: `https://example.test/seg${index}.ts`,
            filename,
            data_b64: part.toString('base64'),
          }),
        );
      });
      return;
    }
    if (message.type === 'finished') {
      clearTimeout(timer);
      resolve(message);
    }
    if (message.type === 'failed') {
      clearTimeout(timer);
      reject(new Error(`failed: ${message.reason}`));
    }
  });
  ws.addEventListener('error', () => reject(new Error('ws hatası')));
});

ws.addEventListener('open', () => {
  ws.send(JSON.stringify({ type: 'hello', protocol: 1, client: 'capture-e2e' }));
});

try {
  const finished = await done;
  console.log('olaylar:', [...new Set(messages)].join(', '));
  console.log('app çıktısı:', finished.path, finished.size, 'byte');

  const written = fs.readFileSync(finished.path);
  const hash = createHash('sha256').update(written).digest('hex');
  const ok = hash === expectedHash && written.length === expected.length;
  console.log(`sha256 eşleşiyor: ${ok ? '✅ evet' : '❌ hayır'} (${written.length} byte)`);
  if (!ok) process.exitCode = 1;
  fs.rmSync(finished.path, { force: true });
} catch (error) {
  console.error(`HATA: ${error.message}`);
  process.exitCode = 1;
} finally {
  ws.close();
}
