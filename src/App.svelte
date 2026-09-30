<script lang="ts">
  import { invoke } from '@tauri-apps/api/core'
  import { listen } from '@tauri-apps/api/event'
  import { open as openFolder, save } from '@tauri-apps/plugin-dialog'

  interface ProbeInfo {
    requested_url: string
    final_url: string
    len: number | null
    accept_ranges: boolean
    etag: string | null
    last_modified: string | null
    content_type: string | null
    filename_hint: string | null
  }

  type Progress =
    | { kind: 'probing'; url: string }
    | { kind: 'planned'; size: number; connections: number; resumed: boolean; bytes_done: number }
    | { kind: 'part_progress'; index: number; part_written: number; part_length: number; total_written: number; total_size: number }
    | { kind: 'hls_planned'; segments: number; variant: string | null; resumed: boolean; done: number }
    | { kind: 'hls_segment'; done: number; total: number; bytes: number }
    | { kind: 'retrying'; index: number; attempt: number; reason: string }
    | { kind: 'falldown_single'; reason: string }
    | { kind: 'assembling'; parts: number }
    | { kind: 'verifying' }
    | { kind: 'finished'; bytes: number; elapsed_ms: number; sha256: string | null }
    | { kind: 'failed'; reason: string }

  interface Outcome {
    path: string
    size: number
    sha256: string | null
    connections: number
    resumed: boolean
    elapsedMs: number
  }

  interface CaptureStatus {
    listening: boolean
    port: number | null
    extensionClients: number
    captured: number
    downloadDir: string
  }

  interface QueueEntry {
    id: string
    url: string
    kind: string
    state: string
    source: string
    written: number
    total: number
    filename: string | null
    path: string | null
    error: string | null
  }

  let url = $state('')
  let dest = $state('')
  let connections = $state(8)
  let sha = $state('')
  let speedLimit = $state(0)
  let appSettings = $state<Record<string, unknown> | null>(null)
  let extensionDir = $state<string | null>(null)
  let info = $state<ProbeInfo | null>(null)
  let written = $state(0)
  let total = $state(0)
  let speed = $state(0)
  let phase = $state<'idle' | 'probing' | 'download' | 'assembling' | 'verifying' | 'done' | 'error'>('idle')
  let log = $state<string[]>([])
  let busy = $state(false)

  let status = $state<CaptureStatus | null>(null)
  let queue = $state<QueueEntry[]>([])

  let lastBytes = 0
  let lastAt = Date.now()

  const percent = $derived(total > 0 ? Math.min(100, (written / total) * 100) : 0)
  const captured = $derived(queue.filter((entry) => entry.source === 'extension'))

  const human = (bytes: number) => {
    const units = ['B', 'KB', 'MB', 'GB', 'TB']
    let value = bytes || 0
    let unit = 0
    while (value >= 1024 && unit < units.length - 1) {
      value /= 1024
      unit += 1
    }
    return `${unit === 0 ? value : value.toFixed(1)} ${units[unit]}`
  }

  const ratio = (entry: QueueEntry) =>
    entry.total > 0 ? Math.min(100, (entry.written / entry.total) * 100) : 0

  function note(line: string) {
    log = [...log.slice(-200), line]
  }

  async function refreshCapture() {
    try {
      status = await invoke<CaptureStatus>('capture_status')
      queue = await invoke<QueueEntry[]>('capture_queue')
      if (!appSettings) appSettings = await invoke<Record<string, unknown>>('capture_settings_get')
      if (!extensionDir) {
        extensionDir = await invoke<string>('extension_path').catch(() => null)
      }
    } catch (error) {
      console.error('capture status failed', error)
    }
  }

  async function saveSettings(patch: Record<string, unknown>) {
    if (!appSettings) appSettings = await invoke<Record<string, unknown>>('capture_settings_get')
    appSettings = { ...appSettings, ...patch }
    appSettings = await invoke<Record<string, unknown>>('capture_settings_set', { settings: appSettings })
    note('ayarlar kaydedildi')
  }

  async function exportExtension() {
    try {
      const picked = await openFolder({
        directory: true,
        multiple: false,
        title: 'Eklentiyi kaydetmek için klasör seç',
      })
      if (typeof picked !== 'string') return
      const path = await invoke<string>('extension_export', { destDir: picked })
      extensionDir = path
      note(`eklenti kopyalandı: ${path}`)
    } catch (error) {
      note(`kopyalanamadı: ${error}`)
    }
  }

  /** Ham motor hatalarını kullanıcıya anlaşılır cümleye çevirir. */
  function humanize(message: string) {
    if (/403/.test(message)) {
      return `bu bağlantı yalnızca oynatıcı oturumunda geçerli — videoyu oynatıp eklenti popup'ından "Segmentleri indir" seçeneğini kullan`
    }
    if (/404/.test(message) && /(\.m3u8|\.mpd|l\.php)/.test(message)) {
      return 'stream bağlantısının süresi dolmuş (oynatıcı token\'ı) — tarayıcıda videoyu yeniden başlatıp tekrar gönder'
    }
    if (/not an HLS playlist/.test(message)) {
      return 'stream tarayıcıda şifreli çözülüyor (client-side) — indirilemiyor'
    }
    if (/SAMPLE-AES|widevine|playready/i.test(message)) return 'DRM korumalı — indirilemiyor'
    if (/timed out|timeout/i.test(message)) return `zaman aşımı: ${message}`
    return message
  }

  async function revealExtension() {
    try {
      const path = await invoke<string>('extension_reveal')
      note(`eklenti klasörü açıldı: ${path}`)
    } catch (error) {
      note(`klasör açılamadı: ${error}`)
    }
  }

  async function copyExtensionPath() {
    if (!extensionDir) return
    try {
      await navigator.clipboard.writeText(extensionDir)
      note('eklenti klasörü yolu kopyalandı')
    } catch (error) {
      note(`kopyalanamadı: ${error}`)
    }
  }

  async function cancelAll() {
    const dropped = await invoke<number>('capture_cancel_all')
    note(`${dropped} iş iptal edildi`)
    await refreshCapture()
  }

  async function cancelCapture(id: string) {
    await invoke('capture_cancel', { id })
    await refreshCapture()
  }

  async function onProbe() {
    if (!url) return
    phase = 'probing'
    info = null
    try {
      const result = await invoke<ProbeInfo>('engine_probe', { url })
      info = result
      if (!dest && result.filename_hint) dest = result.filename_hint
      note(`probe: ${result.len ?? '?'} bytes, ranges=${result.accept_ranges ? 'yes' : 'no'}`)
    } catch (error) {
      note(`probe failed: ${error}`)
    } finally {
      phase = 'idle'
    }
  }

  async function pickDest() {
    const picked = await save({ defaultPath: dest || undefined })
    if (typeof picked === 'string') dest = picked
  }

  async function start() {
    if (!url || !dest || busy) return
    busy = true
    written = 0
    total = 0
    speed = 0
    phase = 'download'
    try {
      // Kuyruğa ekle: ilerleme, iptal ve zamanlama aynı yoldan işler.
      const id = await invoke<string>('queue_add', {
        url,
        dest,
        connections,
        sha256: sha.trim() ? sha.trim() : null,
        speedLimitMbps: speedLimit > 0 ? speedLimit : null,
      })
      phase = 'done'
      note(`kuyruğa eklendi: ${id}`)
      await refreshCapture()
    } catch (error) {
      phase = 'error'
      note(`failed: ${error}`)
    } finally {
      busy = false
    }
  }

  $effect(() => {
    const pending = listen<Progress>('download-progress', (event) => {
      const ev = event.payload
      switch (ev.kind) {
        case 'planned':
          total = ev.size
          written = ev.bytes_done
          lastBytes = ev.bytes_done
          lastAt = Date.now()
          note(ev.resumed ? `resuming with ${ev.connections} parts` : `starting: ${human(ev.size)} in ${ev.connections} parts`)
          break
        case 'part_progress': {
          const now = Date.now()
          if (now - lastAt > 250) {
            speed = (ev.total_written - lastBytes) / ((now - lastAt) / 1000)
            lastBytes = ev.total_written
            lastAt = now
          }
          total = ev.total_size || total
          written = ev.total_written
          break
        }
        case 'hls_planned':
          phase = 'download'
          note(`hls: ${ev.segments} segments${ev.variant ? ` · ${ev.variant}` : ''}`)
          break
        case 'hls_segment':
          total = ev.total
          written = ev.done
          note(`segments ${ev.done}/${ev.total} · ${human(ev.bytes)}`)
          break
        case 'falldown_single':
          note(`single connection: ${ev.reason}`)
          break
        case 'retrying':
          note(`retry ${ev.attempt} on part ${ev.index}: ${ev.reason}`)
          break
        case 'assembling':
          phase = 'assembling'
          note(`assembling ${ev.parts} parts`)
          break
        case 'verifying':
          phase = 'verifying'
          note('verifying checksum')
          break
        case 'finished':
          note(`finished in ${(ev.elapsed_ms / 1000).toFixed(1)}s`)
          break
        case 'failed':
          note(`failed: ${ev.reason}`)
          break
      }
    })
    return () => {
      pending.then((unlisten) => unlisten())
    }
  })

  $effect(() => {
    const statusEvents = listen<CaptureStatus>('capture-status', (event) => {
      status = event.payload
    })
    const queueEvents = listen<QueueEntry[]>('capture-event', (event) => {
      queue = event.payload
    })
    const timer = setInterval(refreshCapture, 2500)
    refreshCapture()
    return () => {
      clearInterval(timer)
      statusEvents.then((unlisten) => unlisten())
      queueEvents.then((unlisten) => unlisten())
    }
  })
</script>

<div class="app-wrapper">
  <header class="header">
    <div class="brand">
      <div class="logo-badge">H</div>
      <div class="brand-text">
        <h1>Hazar</h1>
        <p>multi-connection download engine</p>
      </div>
    </div>
    <div class="capture-pill" class:on={status?.extensionClients}>
      {#if status?.port}
        {status.extensionClients > 0 ? `extension bağlı · :${status.port}` : `köprü hazır · :${status.port}`}
      {:else}
        köprü başlatılıyor…
      {/if}
    </div>
  </header>

  <main class="main-content">
    <section class="card">
      <div class="form-group">
        <label class="form-label" for="url">URL</label>
        <div class="input-group">
          <input id="url" class="file-input" bind:value={url} placeholder="https://example.com/big.iso · .m3u8" />
          <button class="browse-btn" onclick={onProbe} disabled={busy || !url}>Probe</button>
        </div>
        <p class="field-hint">
          {#if info}
            {human(info.len ?? 0)} · range requests {info.accept_ranges ? 'supported' : 'not supported'} · {info.content_type ?? 'unknown type'}
          {:else}
            HEAD + ranged GET probe, ya da .m3u8 girip HLS indir
          {/if}
        </p>
      </div>

      <div class="form-group">
        <label class="form-label" for="dest">Save as</label>
        <div class="input-group">
          <input id="dest" class="file-input" bind:value={dest} placeholder="choose a destination" />
          <button class="browse-btn" onclick={pickDest} disabled={busy}>Browse</button>
        </div>
      </div>

      <div class="content-grid">
        <div class="form-group">
          <label class="form-label" for="connections">Connections</label>
          <input id="connections" class="file-input" type="number" min="1" max="16" bind:value={connections} disabled={busy} />
          <p class="field-hint">1–16, default 8.</p>
        </div>
        <div class="form-group">
          <label class="form-label" for="sha">Verify SHA-256 (optional)</label>
          <input id="sha" class="file-input" bind:value={sha} placeholder="hex digest" disabled={busy} />
          <p class="field-hint">Uyuşmazsa indirme hata verir.</p>
        </div>
        <div class="form-group">
          <label class="form-label" for="speed">Hız limiti (MB/s)</label>
          <input id="speed" class="file-input" type="number" min="0" step="0.5" bind:value={speedLimit} disabled={busy} />
          <p class="field-hint">0 = sınırsız. Tüm connection'lar toplam bu hızı aşmaz.</p>
        </div>
      </div>

      <div class="button-group">
        <button class="browse-btn" onclick={start} disabled={busy || !url || !dest}>
          {busy ? 'Ekleniyor…' : 'Kuyruğa ekle'}
        </button>
        <button class="browse-btn" onclick={cancelAll}>Tümünü iptal</button>
      </div>

      {#if phase !== 'idle' || total > 0}
        <div class="progress">
          <div class="progress-bar" style={`width: ${percent}%`}></div>
        </div>
        <p class="field-hint">
          {phase} · {human(written)}{total > 0 ? ` / ${human(total)}` : ''} · {percent.toFixed(1)}%
          {#if speed > 0} · {human(speed)}/s{/if}
        </p>
      {/if}
    </section>

    <section class="card">
      <div class="form-group">
        <div class="form-label">Tarayıcı eklentisi (unpacked)</div>
        <p class="field-hint">
          Store'da yayınlanmıyor; eklenti klasörü uygulamayla birlikte gelir:
        </p>
        <p class="path selectable">{extensionDir ?? 'bulunamadı'}</p>
        <div class="button-group">
          <button class="browse-btn primary" onclick={exportExtension}>Extension'ı indir…</button>
          <button class="browse-btn" onclick={revealExtension} disabled={!extensionDir}>Klasörü göster</button>
          <button class="browse-btn" onclick={copyExtensionPath} disabled={!extensionDir}>Yolu kopyala</button>
        </div>
        <p class="field-hint">
          <b>Chrome / Edge:</b> <code>chrome://extensions</code> → Geliştirici modu aç →
          “Paketlenmemiş öğe yükle” → bu klasörü seç.<br />
          <b>Firefox:</b> <code>about:debugging</code> → “Geçici Eklenti Yükle” → klasördeki
          <code>manifest.json</code>. Eklenti popup'ında “bağlı · :8722” görünmeli.
        </p>
      </div>
    </section>

    <section class="card">
      <div class="form-group">
        <label class="form-label" for="schedule">Gece indirme penceresi</label>
        <div class="content-grid">
          <div class="input-group">
            <input
              id="schedule"
              type="checkbox"
              checked={Boolean(appSettings?.schedule_enabled)}
              onchange={(event) => saveSettings({ schedule_enabled: event.currentTarget.checked })}
            />
            <span class="field-hint">kapalıysa hep indirir</span>
          </div>
          <div class="input-group">
            <input
              class="file-input"
              value={String(appSettings?.schedule_from ?? '02:00')}
              onchange={(event) => saveSettings({ schedule_from: event.currentTarget.value })}
            />
            <span class="field-hint">–</span>
            <input
              class="file-input"
              value={String(appSettings?.schedule_to ?? '08:00')}
              onchange={(event) => saveSettings({ schedule_to: event.currentTarget.value })}
            />
          </div>
        </div>
        <p class="field-hint">
          Kuyruk bu pencerenin dışında "scheduled" kalır; pencere açılınca otomatik başlar
          (max eşzamanlı: {String(appSettings?.max_concurrent_downloads ?? 3)}).
        </p>
      </div>

      <div class="form-group">
        <div class="form-label">Kuyruk ({queue.length}) — yakalanan: {captured.length}</div>
        <p class="field-hint">Klasör: {status?.downloadDir ?? '—'}</p>
      </div>

      {#if captured.length === 0}
        <p class="field-hint">Henüz yakalanan indirme yok. Extension'ı yükleyip bir indirme başlat.</p>
      {:else}
        {#each captured.slice(-8).reverse() as entry (entry.id)}
          <div class="queue-row">
            <div class="queue-main">
              <div class="queue-title">
                <span class="queue-kind">{entry.kind}</span>
                {entry.filename ?? entry.url}
              </div>
              <div class="progress small">
                <div class="progress-bar" style={`width: ${ratio(entry)}%`}></div>
              </div>
              <p class="field-hint">
                {entry.state}{entry.total > 0 ? ` · ${human(entry.written)} / ${human(entry.total)}` : ''}
                {#if entry.error} · {humanize(entry.error)}{/if}
              </p>
            </div>
            {#if entry.state === 'downloading' || entry.state === 'queued'}
              <button class="browse-btn" onclick={() => cancelCapture(entry.id)}>İptal</button>
            {/if}
          </div>
        {/each}
      {/if}
    </section>

    {#if log.length}
      <section class="log-section">
        <div class="log-container">
          {#each log as line}
            <div class="log-line">{line}</div>
          {/each}
        </div>
      </section>
    {/if}
  </main>
</div>

<style>
  .progress {
    height: 6px;
    border-radius: 999px;
    background: rgba(255, 255, 255, 0.08);
    overflow: hidden;
    margin-top: 14px;
  }

  .progress.small {
    margin-top: 6px;
    height: 4px;
  }

  .progress-bar {
    height: 100%;
    background: var(--accent);
    transition: width 0.2s ease;
  }

  .log-line {
    font-family: var(--mono);
    font-size: 11px;
    line-height: 1.6;
    color: var(--text-2);
    white-space: pre-wrap;
    word-break: break-all;
  }

  .path.selectable {
    user-select: text;
    -webkit-user-select: text;
    cursor: text;
  }

  .path {
    font-family: var(--mono);
    font-size: 11px;
    color: var(--text-2);
    background: rgba(0, 0, 0, 0.22);
    border: 1px solid var(--border);
    border-radius: 6px;
    padding: 6px 8px;
    word-break: break-all;
    margin: 4px 0 8px;
  }

  .capture-pill {
    font-size: 11px;
    padding: 4px 10px;
    border-radius: 999px;
    border: 1px solid var(--border);
    color: var(--text-2);
  }

  .capture-pill.on {
    color: var(--text);
    border-color: var(--accent);
  }

  .queue-row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 0;
    border-top: 1px solid var(--separator);
  }

  .queue-main {
    min-width: 0;
    flex: 1;
  }

  .queue-title {
    font-size: 12px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .queue-kind {
    display: inline-block;
    font-size: 10px;
    padding: 1px 6px;
    margin-right: 6px;
    border-radius: 4px;
    background: rgba(99, 102, 241, 0.18);
    color: #a5b4fc;
  }
</style>
