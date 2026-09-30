<script lang="ts">
  import { onMount } from 'svelte'
  import { isTauri, invoke } from '@tauri-apps/api/core'
  import { listen } from '@tauri-apps/api/event'
  import { open as chooseFolder } from '@tauri-apps/plugin-dialog'
  import { revealItemInDir } from '@tauri-apps/plugin-opener'
  import { check } from '@tauri-apps/plugin-updater'
  import { relaunch } from '@tauri-apps/plugin-process'

  type Job = { id: string; url: string; filename: string | null; state: string; written: number; total: number; path: string | null; error: string | null }
  type Settings = { download_dir: string | null; connections: number; max_concurrent_downloads: number; capture_enabled: boolean; min_size_bytes: number; excluded_hosts: string[]; schedule_enabled: boolean; schedule_from: string; schedule_to: string }
  type Status = { extensionClients: number; downloadDir: string }
  let jobs = $state<Job[]>([])
  let settings = $state<Settings | null>(null)
  let status = $state<Status | null>(null)
  let url = $state('')
  let message = $state('')
  let adding = $state(false)
  let showSettings = $state(false)
  let version = $state('')
  let updating = $state(false)
  let refreshId = $state('')
  let refreshUrl = $state('')
  const labels: Record<string, string> = { queued: 'Bekliyor', scheduled: 'Zamanlandı', downloading: 'İndiriliyor', paused: 'Duraklatıldı', interrupted: 'Yarım kaldı', cancelled: 'İptal edildi', cancelling: 'Durduruluyor', failed: 'Başarısız', done: 'Tamamlandı', needs_refresh: 'Yeni link gerekli' }
  const active = (job: Job) => ['queued', 'scheduled', 'downloading'].includes(job.state)
  const resumable = (job: Job) => ['paused', 'interrupted', 'cancelled', 'failed', 'needs_refresh'].includes(job.state)
  const size = (value: number) => {
    const units = ['B', 'KB', 'MB', 'GB']; let index = 0
    while (value >= 1024 && index < units.length - 1) { value /= 1024; index++ }
    return `${value.toFixed(index ? 1 : 0)} ${units[index]}`
  }
  const percent = (job: Job) => job.total > 0 ? Math.min(100, job.written / job.total * 100) : 0
  async function refresh() {
    [jobs, status] = await Promise.all([invoke<Job[]>('capture_queue'), invoke<Status>('capture_status')])
  }
  async function action(command: string, args?: Record<string, unknown>) {
    message = ''
    try { await invoke(command, args); await refresh() } catch (error) { message = String(error) }
  }
  async function add() {
    if (adding || !url.trim()) return
    try {
      const parsed = new URL(url.trim())
      if (!['http:', 'https:'].includes(parsed.protocol)) throw new Error('HTTP/HTTPS linki gerekli')
      adding = true; message = ''
      await invoke('queue_add', { url: parsed.href, dest: '', connections: null, sha256: null, speedLimitMbps: null })
      url = ''; await refresh()
    } catch (error) { message = String(error) } finally { adding = false }
  }
  async function save(patch: Partial<Settings>) {
    if (!settings) return
    try { settings = await invoke<Settings>('capture_settings_set', { settings: { ...settings, ...patch } }); await refresh() }
    catch (error) { message = String(error) }
  }
  async function folder() {
    const path = await chooseFolder({ directory: true, multiple: false })
    if (typeof path === 'string') await save({ download_dir: path })
  }
  async function exportExtension() {
    const path = await chooseFolder({ directory: true, multiple: false, title: 'Eklentiyi kaydet' })
    if (typeof path === 'string') await action('extension_export', { destDir: path })
  }
  async function update() {
    updating = true; message = ''
    try { const release = await check(); if (release) { await release.downloadAndInstall(); await relaunch() } else message = 'Güncel versiyon kurulu.' }
    catch (error) { message = String(error) } finally { updating = false }
  }
  onMount(() => {
    if (isTauri() && /Mac/.test(navigator.platform)) document.documentElement.classList.add('native-vibrancy')
    let disposed = false
    const unlisteners: (() => void)[] = []
    void Promise.all([invoke<Settings>('capture_settings_get'), invoke<string>('engine_version')]).then(([s, v]) => { if (!disposed) { settings = s; version = v } }).catch(e => message = String(e))
    for (const event of ['capture-event', 'capture-status']) {
      void listen(event, () => { void refresh().catch(e => message = String(e)) }).then(unlisten => { if (disposed) unlisten(); else unlisteners.push(unlisten) })
    }
    void refresh().catch(e => message = String(e))
    const timer = setInterval(() => { void refresh().catch(() => {}) }, 2000)
    return () => { disposed = true; clearInterval(timer); unlisteners.forEach(fn => fn()) }
  })
</script>

<main>
  <header data-tauri-drag-region>
    <div class="brand"><img src="/logo.png" alt="" width="32" height="32" /><div><h1>Hazar <span>{version}</span></h1><p>İndirmelerin tek yerde.</p></div></div>
    <button class:chosen={showSettings} onclick={() => showSettings = !showSettings}>Ayarlar</button>
  </header>
  <form class="add" onsubmit={(event) => { event.preventDefault(); void add() }}>
    <label class="sr-only" for="url">Download linki</label>
    <input id="url" type="url" bind:value={url} placeholder="Download linkini yapıştır" required autocomplete="off" />
    <button class="primary" disabled={adding || !url.trim()}>{adding ? 'Ekleniyor…' : 'İndir'}</button>
  </form>
  {#if message}<p class="notice" role="status">{message}</p>{/if}
  {#if showSettings && settings}
    <section class="settings" aria-label="Ayarlar">
      <div class="setting"><div><strong>Download klasörü</strong><p>{settings.download_dir ?? status?.downloadDir ?? 'Downloads'}</p></div><button onclick={folder}>Değiştir</button></div>
      <div class="setting"><label for="concurrent">Aynı anda indir</label><select id="concurrent" value={settings.max_concurrent_downloads} onchange={(e) => save({ max_concurrent_downloads: Number(e.currentTarget.value) })}>{#each [1, 2, 3, 4] as count}<option value={count}>{count} dosya</option>{/each}</select></div>
      <div class="setting"><label for="capture">Tarayıcı indirmelerini yakala</label><input id="capture" type="checkbox" checked={settings.capture_enabled} onchange={(e) => save({ capture_enabled: e.currentTarget.checked })} /></div>
      <div class="setting"><div><strong>Tarayıcı eklentisi</strong><p>{status?.extensionClients ? 'Bağlı' : 'Bağlı değil'} · Chrome / Edge / Firefox</p></div><button onclick={exportExtension}>Eklentiyi kaydet</button></div>
      <p class="hint">Chrome/Edge: Extensions → Developer mode → Load unpacked. Kaydedilen klasörü seç.</p>
      <div class="setting"><span>Hazar {version}</span><button disabled={updating} onclick={update}>{updating ? 'Güncelleniyor…' : 'Update kontrol et'}</button></div>
    </section>
  {/if}
  <section class="downloads" aria-label="İndirmeler">
    <div class="list-title"><h2>İndirmeler</h2><span>{jobs.filter(active).length} aktif</span></div>
    {#if !jobs.length}
      <div class="empty"><h3>İlk download’unu ekle.</h3><p>Bir link yapıştır veya tarayıcı eklentisinden gönder.</p></div>
    {/if}
    {#each [...jobs].reverse() as job (job.id)}
      <article class="job">
        <div class="job-heading"><strong title={job.filename ?? ''}>{job.filename ?? 'Download'}</strong><span class:done={job.state === 'done'}>{labels[job.state] ?? job.state}</span></div>
        {#if active(job)}<progress max="100" value={job.total > 0 ? percent(job) : undefined} aria-label="Download ilerlemesi"></progress>{/if}
        <div class="job-footer"><span>{size(job.written)}{job.total > 0 ? ` / ${size(job.total)}` : ''}</span><div class="actions">
          {#if active(job)}<button onclick={() => action('queue_pause', { id: job.id })}>Duraklat</button>{/if}
          {#if resumable(job)}<button onclick={() => action('queue_resume', { id: job.id, url: null })}>Devam et</button><button onclick={() => { refreshId = job.id; refreshUrl = '' }}>Link yenile</button>{/if}
          {#if job.state === 'done' && job.path}<button onclick={() => { void revealItemInDir(job.path!).catch(e => message = String(e)) }}>Klasörde göster</button>{/if}
        </div></div>
        {#if job.error && job.state !== 'paused'}<p class="error">{job.error}</p>{/if}
        {#if refreshId === job.id}<form class="refresh" onsubmit={(e) => { e.preventDefault(); void action('queue_resume', { id: job.id, url: refreshUrl }).then(() => refreshId = '') }}><input type="url" bind:value={refreshUrl} placeholder="Yeni download linki" required aria-label="Yeni download linki" /><button>Devam et</button><button type="button" onclick={() => refreshId = ''}>Vazgeç</button></form>{/if}
      </article>
    {/each}
  </section>
  <footer><span class:connected={!!status?.extensionClients}>●</span> Tarayıcı {status?.extensionClients ? 'bağlı' : 'bağlı değil'}</footer>
</main>
