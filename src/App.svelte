<script lang="ts">
  import { onMount } from 'svelte'
  import { isTauri, invoke } from '@tauri-apps/api/core'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import { listen } from '@tauri-apps/api/event'
  import { open as chooseFolder } from '@tauri-apps/plugin-dialog'
  import { revealItemInDir } from '@tauri-apps/plugin-opener'
  import { check } from '@tauri-apps/plugin-updater'
  import { relaunch } from '@tauri-apps/plugin-process'
  import { locales, type Lang } from './locales'

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
  let showUrl = $state(false)
  let version = $state('')
  let updating = $state(false)
  let refreshId = $state('')
  let refreshUrl = $state('')
  const storedLang = (() => { try { return localStorage.getItem('lang') } catch { return null } })()
  let lang = $state<Lang>(storedLang && storedLang in locales ? storedLang as Lang : 'en')
  const t = $derived(locales[lang].messages)
  $effect(() => { document.documentElement.lang = lang })
  function setLang(value: Lang) { lang = value; try { localStorage.setItem('lang', value) } catch {} }
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
      if (!['http:', 'https:'].includes(parsed.protocol)) throw new Error(t.httpRequired)
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
    const path = await chooseFolder({ directory: true, multiple: false, title: t.saveExtension })
    if (typeof path === 'string') await action('extension_export', { destDir: path })
  }
  async function update() {
    updating = true; message = ''
    try { const release = await check(); if (release) { await release.downloadAndInstall(); await relaunch() } else message = t.upToDate }
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
    const dragHeader = (event: MouseEvent) => {
      const target = event.target as Element;
      if (event.button !== 0 || !target.closest('header') || target.closest('button, input, select, a') || !isTauri()) return;
      event.preventDefault();
      void getCurrentWindow().startDragging().catch(e => message = String(e));
    }
    document.addEventListener('mousedown', dragHeader);
    const timer = setInterval(() => { void refresh().catch(() => {}) }, 2000)
    return () => { document.removeEventListener('mousedown', dragHeader); disposed = true; clearInterval(timer); unlisteners.forEach(fn => fn()) }
  })
</script>

<main>
  <header>
    <div class="brand"><img src="/logo.png" alt="" width="32" height="32" /><div><h1>Hazar Download Manager <span>{version}</span></h1><p>{t.tagline}</p></div></div>
    <div class="header-actions"><button class:chosen={showUrl} aria-expanded={showUrl} aria-controls="manual-download" onclick={() => showUrl = !showUrl}>{t.addUrl}</button><button class:chosen={showSettings} onclick={() => showSettings = !showSettings}>{t.settings}</button></div>
  </header>
  <div class="workspace">
  {#if showUrl}
  <form id="manual-download" class="add" onsubmit={(event) => { event.preventDefault(); void add() }}>
    <label class="sr-only" for="url">{t.downloadLink}</label>
    <input id="url" type="url" bind:value={url} placeholder={t.pasteLink} required autocomplete="off" />
    <button class="primary" disabled={adding || !url.trim()}>{adding ? t.adding : t.download}</button>
  </form>
  {/if}
  {#if message}<p class="notice" role="status">{message}</p>{/if}
  {#if showSettings && settings}
    <section class="settings" aria-label={t.settings}>
      <div class="setting"><div><strong>{t.downloadFolder}</strong><p>{settings.download_dir ?? status?.downloadDir ?? 'Downloads'}</p></div><button onclick={folder}>{t.change}</button></div>
      <div class="setting"><label for="concurrent">{t.concurrent}</label><select id="concurrent" value={settings.max_concurrent_downloads} onchange={(e) => save({ max_concurrent_downloads: Number(e.currentTarget.value) })}>{#each [1, 2, 3, 4] as count}<option value={count}>{t.files(count)}</option>{/each}</select></div>
      <div class="setting"><label for="capture">{t.captureBrowser}</label><input id="capture" type="checkbox" checked={settings.capture_enabled} onchange={(e) => save({ capture_enabled: e.currentTarget.checked })} /></div>
      <div class="setting"><label for="language">{t.language}</label><select id="language" value={lang} onchange={(e) => setLang(e.currentTarget.value as Lang)}>{#each Object.entries(locales) as [code, locale]}<option value={code}>{locale.name}</option>{/each}</select></div>
      <div class="setting"><div><strong>{t.extension}</strong><p>{status?.extensionClients ? t.connected : t.notConnected} · Chrome / Edge / Firefox</p></div><button onclick={exportExtension}>{t.saveExtension}</button></div>
      <p class="hint">{t.extensionHint}</p>
      <div class="setting"><span>Hazar Download Manager {version}</span><button disabled={updating} onclick={update}>{updating ? t.updating : t.checkUpdate}</button></div>
    </section>
  {/if}
  <section class="downloads" aria-label={t.downloads}>
    <div class="list-title"><h2>{t.downloads}</h2><div class="list-tools"><span>{t.active(jobs.filter(job => active(job) || job.state === 'assembling').length)}</span><button disabled={!jobs.length} onclick={() => action('queue_clear')}>{t.clearList}</button></div></div>
    {#if !jobs.length}
      <div class="empty"><h3>{t.emptyTitle}</h3><p>{t.emptyText}</p></div>
    {/if}
    {#each [...jobs].reverse() as job (job.id)}
      <article class="job">
        <div class="job-heading"><strong title={job.filename ?? ''}>{job.filename ?? 'Download'}</strong><span class:done={job.state === 'done'}>{t.states[job.state] ?? job.state}</span></div>
        {#if active(job)}<progress max="100" value={job.total > 0 ? percent(job) : undefined} aria-label={t.progress}></progress>{/if}
        <div class="job-footer"><span>{size(job.written)}{job.total > 0 ? ` / ${size(job.total)}` : ''}</span><div class="actions">
          {#if active(job)}<button onclick={() => action('queue_pause', { id: job.id })}>{t.pause}</button>{/if}
          {#if resumable(job)}<button onclick={() => action('queue_resume', { id: job.id, url: null })}>{t.resume}</button><button onclick={() => { refreshId = job.id; refreshUrl = '' }}>{t.refreshLink}</button>{/if}
          {#if job.state === 'done' && job.path}<button onclick={() => { void revealItemInDir(job.path!).catch(e => message = String(e)) }}>{t.showInFolder}</button>{/if}
          <button class="remove" aria-label={t.removeLabel(job.filename ?? 'Download')} onclick={() => action('queue_remove', { id: job.id })}>{t.remove}</button>
        </div></div>
        {#if job.error && job.state !== 'paused'}<p class="error">{job.error}</p>{/if}
        {#if refreshId === job.id}<form class="refresh" onsubmit={(e) => { e.preventDefault(); void action('queue_resume', { id: job.id, url: refreshUrl }).then(() => refreshId = '') }}><input type="url" bind:value={refreshUrl} placeholder={t.newLink} required aria-label={t.newLink} /><button>{t.resume}</button><button type="button" onclick={() => refreshId = ''}>{t.cancel}</button></form>{/if}
      </article>
    {/each}
  </section>
  </div>
  <footer><span class:connected={!!status?.extensionClients}>●</span> {status?.extensionClients ? t.browserConnected : t.browserNotConnected}</footer>
</main>
