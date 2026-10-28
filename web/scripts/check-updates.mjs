import assert from 'node:assert/strict'
import { mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const outDir = join(root, 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })
const bundle = join(outDir, 'updates-store.mjs')
await build({
  entryPoints: [join(root, 'src', 'updateStore.ts')], outfile: bundle,
  bundle: true, format: 'esm', platform: 'node', target: 'node20',
  external: ['react'], logLevel: 'silent',
})

const nativeFetch = globalThis.fetch
const nativeSetTimeout = globalThis.setTimeout
const nativeClearTimeout = globalThis.clearTimeout
const nativeWindow = globalThis.window
let timers = new Map()
let nextTimer = 0
let requests = []
let scenario = 0
const current = {
  currentVersion: '0.4.2-beta', latest: {
    version: '0.4.3-beta', tag: 'v0.4.3-beta', name: 'Denia 0.4.3-beta', body: 'Release notes',
    htmlUrl: 'https://github.com/example/denia/releases/tag/v0.4.3-beta',
    publishedAt: '2026-10-15T12:00:00Z', prerelease: true,
    installer: { name: 'Denia-0.4.3-beta-setup.exe', size: 2048 },
  },
  updateAvailable: true, canInstall: true, checkedAt: 1792065600000,
  repositoryUrl: 'https://github.com/example/denia',
}
const idle = { phase: 'idle', version: null, downloadedBytes: 0, totalBytes: null, error: null }
const downloading = { ...idle, phase: 'downloading', version: '0.4.3-beta', totalBytes: 2048, downloadedBytes: 512 }
const reply = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })
const tick = async () => { for (let i = 0; i < 20; i++) await Promise.resolve() }
function deferred() {
  let resolve
  let reject
  const promise = new Promise((a, b) => { resolve = a; reject = b })
  return { promise, resolve, reject }
}
async function fresh(route) {
  timers = new Map()
  requests = []
  globalThis.window = new EventTarget()
  globalThis.setTimeout = (callback, ms) => {
    const id = ++nextTimer
    timers.set(id, { callback, ms })
    return id
  }
  globalThis.clearTimeout = (id) => { timers.delete(id) }
  globalThis.fetch = async (url, init) => {
    requests.push({ url, init })
    return route(url, init)
  }
  return import(`${pathToFileURL(bundle).href}?scenario=${++scenario}`)
}
const polls = () => [...timers.values()].filter((timer) => timer.ms === 800)

try {
  {
    const check = deferred()
    const mod = await fresh((url) => url.startsWith('/api/update/check') ? check.promise : reply(idle))
    mod.ensureStartupUpdateCheck()
    mod.ensureStartupUpdateCheck()
    const pending = mod.checkForUpdates(true)
    assert.equal(requests.filter((r) => r.url.startsWith('/api/update/check')).length, 1, 'StrictMode and manual checks share a single request')
    assert.ok([...timers.values()].some((t) => t.ms === 25000), 'check has a 25-second timeout')
    assert.equal(mod.getUpdateState().dialogOpen, false, 'startup check is silent')
    check.resolve(reply(current))
    await pending
    await tick()
    assert.equal(mod.getUpdateState().checking, false)
    assert.equal(mod.getUpdateState().check.latest.version, '0.4.3-beta')
    assert.equal(polls().length, 0, 'idle never starts polling')
    console.log('ok   startup silence, StrictMode/concurrent deduplication, check timeout')
  }
  {
    let attempt = 0
    const mod = await fresh(() => ++attempt === 1
      ? reply({ error: { code: 'github-rate-limit', message: 'GitHub API rate limit exceeded' } }, 429)
      : reply(current))
    await mod.checkForUpdates(true)
    assert.equal(mod.getUpdateState().checkError, 'GitHub API rate limit exceeded')
    assert.equal(mod.getUpdateState().checking, false)
    await mod.checkForUpdates(true)
    assert.equal(mod.getUpdateState().checkError, null)
    assert.ok(requests.every((r) => r.url === '/api/update/check?force=true'))
    console.log('ok   real API errors are preserved, failure permits a forced retry')
  }
  {
    const pending = deferred()
    let status = downloading
    let installRequests = 0
    const mod = await fresh((url) => {
      if (url.startsWith('/api/update/check')) return reply(current)
      if (url === '/api/update/install') return ++installRequests === 1 ? pending.promise : reply(downloading)
      return reply(status)
    })
    await mod.checkForUpdates()
    const first = mod.installUpdate()
    const second = mod.installUpdate()
    assert.equal(first, second, 'double-click shares the installation request')
    assert.equal(requests.filter((r) => r.url === '/api/update/install').length, 1)
    assert.equal(requests.find((r) => r.url === '/api/update/install').init.body, JSON.stringify({ version: '0.4.3-beta' }))
    pending.resolve(reply(downloading))
    await first
    assert.equal(polls().length, 1, 'only one download poll exists')
    mod.closeUpdateDialog()
    assert.equal(mod.getUpdateState().status.downloadedBytes, 512, 'closing dialog keeps cross-page progress')
    status = { ...downloading, phase: 'verifying', downloadedBytes: 2048 }
    await mod.refreshUpdateStatus()
    assert.equal(polls().length, 1)
    status = { ...status, phase: 'launched' }
    await mod.refreshUpdateStatus()
    assert.equal(mod.getUpdateState().status.phase, 'launched')
    assert.equal(polls().length, 0, 'installer handoff stops polling')
    await mod.installUpdate()
    assert.equal(requests.filter((r) => r.url === '/api/update/install').length, 1, 'launched state cannot launch twice')
    status = idle
    mod.openUpdateDialog()
    await tick()
    assert.equal(mod.getUpdateState().status.phase, 'idle', 'opening again reconciles a cancelled or exited installer')
    await mod.installUpdate()
    assert.equal(requests.filter((r) => r.url === '/api/update/install').length, 2, 'installer cancellation can be retried after reconciliation')
    assert.equal(mod.getUpdateState().status.phase, 'downloading')
    console.log('ok   install deduplication, exact version payload, cross-page progress, handoff and installer cancellation')
  }
  {
    let installs = 0
    const mod = await fresh((url) => {
      if (url.startsWith('/api/update/check')) return reply(current)
      if (url === '/api/update/install') return ++installs === 1
        ? reply({ error: { code: 'checksum', message: 'SHA-256 mismatch' } }, 409)
        : reply(downloading)
      return reply(downloading)
    })
    await mod.checkForUpdates()
    await mod.installUpdate()
    assert.equal(mod.getUpdateState().status.phase, 'failed')
    assert.equal(mod.getUpdateState().status.error, 'SHA-256 mismatch')
    assert.equal(polls().length, 0)
    await mod.installUpdate()
    assert.equal(mod.getUpdateState().status.phase, 'downloading')
    assert.equal(installs, 2, 'a confirmed failure permits install retry')
    window.dispatchEvent(new Event('pagehide'))
    assert.equal(polls().length, 0, 'page exit clears the active timer')
    console.log('ok   installation errors remain actionable and retryable; page exit cleans timers')
  }
  {
    let offline = false
    const launching = { ...downloading, phase: 'launching' }
    const mod = await fresh((url) => {
      if (url.startsWith('/api/update/check')) return reply(current)
      if (url === '/api/update/install') return reply(launching)
      if (offline) throw new TypeError('Failed to fetch')
      return reply(launching)
    })
    await mod.checkForUpdates()
    await mod.installUpdate()
    offline = true
    await mod.refreshUpdateStatus()
    assert.equal(mod.getUpdateState().status.phase, 'launching', 'normal installer shutdown must not become a failure')
    assert.ok(mod.getUpdateState().status.error)
    assert.equal(polls().length, 0, 'connection loss does not leave an uncontrolled timer')
    offline = false
    await mod.installUpdate()
    assert.equal(requests.filter((r) => r.url === '/api/update/install').length, 1, 'connection retry reconciles with GET, never duplicates POST')
    assert.equal(mod.getUpdateState().status.error, null)
    console.log('ok   installer disconnect is not failure; reconnect only reads status')
  }
  {
    const oldStatus = deferred()
    const mod = await fresh((url) => {
      if (url.startsWith('/api/update/check')) return reply(current)
      if (url === '/api/update/install') return reply(downloading)
      return oldStatus.promise
    })
    mod.ensureStartupUpdateCheck()
    await mod.checkForUpdates()
    await mod.installUpdate()
    oldStatus.resolve(reply(idle))
    await tick()
    assert.equal(mod.getUpdateState().status.phase, 'downloading', 'stale startup status cannot overwrite a newly started installation')
    console.log('ok   stale startup status does not overwrite new installation')
  }
  {
    let installRequests = 0
    const mod = await fresh((url) => {
      if (url === '/api/update/install') installRequests++
      return reply({ ...current, canInstall: false })
    })
    await mod.checkForUpdates()
    await mod.installUpdate()
    assert.equal(installRequests, 0, 'remote listener can never start install')
    console.log('ok   remote installation is blocked before any POST')
  }
  console.log('updates store checks passed')
} finally {
  globalThis.fetch = nativeFetch
  globalThis.setTimeout = nativeSetTimeout
  globalThis.clearTimeout = nativeClearTimeout
  globalThis.window = nativeWindow
}

const uiBundle = join(outDir, 'updates-ui.mjs')
await build({
  stdin: {
    contents: `
      export * from './src/updateStore.ts'
      export { UpdateDialog } from './src/components/updates/UpdateDialog.tsx'
      export { UpdateBadge } from './src/components/updates/UpdateBadge.tsx'
      export { default as VersionPage } from './src/pages/VersionPage.tsx'
      export { installedVersion } from './src/components/updates/updateText.ts'
      export { setLocale } from './src/i18n.ts'
    `,
    resolveDir: root, loader: 'tsx', sourcefile: 'updates-test-entry.tsx',
  },
  outfile: uiBundle, bundle: true, format: 'esm', platform: 'node', target: 'node20',
  jsx: 'automatic', external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
  loader: { '.css': 'empty' }, logLevel: 'silent',
})

const { JSDOM } = await import('jsdom')
const dom = new JSDOM('<!doctype html><html><body><div id="root"></div></body></html>', {
  url: 'http://localhost/', pretendToBeVisual: true,
})
const g = dom.window
const globals = new Map()
for (const key of ['window', 'document', 'navigator', 'HTMLElement', 'Element', 'Node', 'Event', 'MouseEvent', 'KeyboardEvent', 'FocusEvent', 'getComputedStyle', 'requestAnimationFrame', 'cancelAnimationFrame', 'MutationObserver', 'DOMParser']) {
  globals.set(key, Object.getOwnPropertyDescriptor(globalThis, key))
  Object.defineProperty(globalThis, key, { value: g[key], configurable: true, writable: true })
}
globalThis.IS_REACT_ACT_ENVIRONMENT = true
const React = (await import('react')).default
const { act } = await import('react')
const { createRoot } = await import('react-dom/client')
const H = React.createElement
const mod = await import(pathToFileURL(uiBundle).href)
const doc = g.document
const rootEl = doc.getElementById('root')
const reactRoot = createRoot(rootEl)
const settle = () => new Promise((resolve) => nativeSetTimeout(resolve, 25))
let uiCheck = null
let uiStatus = idle
let posted = 0
let versionNavigations = 0
const click = async (element) => {
  assert.ok(element, 'click target exists')
  await act(async () => {
    element.dispatchEvent(new g.MouseEvent('click', { bubbles: true }))
    await settle()
  })
}
function renderUI() {
  reactRoot.render(H(React.Fragment, null,
    H(mod.VersionPage, { onBack: () => {} }),
    H(mod.UpdateBadge),
    H(mod.UpdateDialog, { onOpenVersion: () => { versionNavigations++ } }),
  ))
}
globalThis.fetch = async (url) => {
  if (url.startsWith('/api/update/check')) return uiCheck ? reply(uiCheck) : reply({ error: { code: 'offline', message: 'Offline fixture' } }, 503)
  if (url === '/api/update/install') { posted++; return reply(downloading) }
  return reply(uiStatus)
}
try {
  await act(async () => { renderUI(); await settle() })
  assert.ok(doc.querySelector('.version-number').textContent.includes(mod.installedVersion), 'offline first check still displays the bundled, real version')
  assert.match(doc.querySelector('.version-update-section').textContent, /Offline fixture/)
  assert.equal(doc.querySelector('.update-badge'), null, 'unknown state never shows a fake update badge')
  console.log('ok   offline version page displays actual local version, actionable check error, no fake update badge')

  const checkButton = [...doc.querySelectorAll('.version-update-actions button')].find((button) => button.textContent.includes('检查更新'))
  checkButton.focus()
  await click(checkButton)
  let dialog = doc.querySelector('[role="dialog"]')
  assert.ok(dialog, 'manual check opens the shared dialog')
  assert.equal(dialog.parentElement.parentElement, doc.body, 'dialog is portalled outside the application root')
  assert.equal(rootEl.getAttribute('aria-hidden'), 'true', 'application background is hidden from assistive technology')
  assert.equal(rootEl.inert, true, 'application background is inert')
  assert.equal(doc.activeElement, dialog, 'opening places focus in the dialog')
  const focusables = [...dialog.querySelectorAll('button, a[href]')].filter((element) => !element.disabled)
  const first = focusables[0]
  const last = focusables.at(-1)
  await act(async () => {
    last.focus()
    doc.dispatchEvent(new g.KeyboardEvent('keydown', { key: 'Tab', bubbles: true, cancelable: true }))
  })
  assert.equal(doc.activeElement, first, 'Tab wraps from last control to first')
  await act(async () => {
    first.focus()
    doc.dispatchEvent(new g.KeyboardEvent('keydown', { key: 'Tab', shiftKey: true, bubbles: true, cancelable: true }))
  })
  assert.equal(doc.activeElement, last, 'Shift+Tab wraps to last control')
  await act(async () => {
    doc.dispatchEvent(new g.KeyboardEvent('keydown', { key: 'Escape', bubbles: true, cancelable: true }))
    await settle()
  })
  assert.equal(doc.querySelector('[role="dialog"]'), null)
  assert.equal(doc.activeElement, checkButton, 'Escape restores the invoking control')
  assert.equal(rootEl.getAttribute('aria-hidden'), null)
  assert.equal(rootEl.inert, undefined, 'previous inert state is restored exactly')
  assert.equal(doc.body.style.overflow, '')
  console.log('ok   shared dialog, portal layering, focus trap, Escape, inert background and focus restoration')

  uiCheck = { ...current, latest: { ...current.latest, publishedAt: null, body: '## Real notes\n\n<script>window.__unsafeUpdate = true</script>\n\n[bad](javascript:alert(1))\n\n- A real improvement' } }
  await act(async () => { await mod.checkForUpdates(true); await settle() })
  assert.ok(doc.querySelector('.update-badge'))
  assert.equal(doc.querySelector('.update-release-meta time'), null, 'null publication date is omitted rather than showing epoch')
  assert.equal(doc.querySelector('.update-markdown script'), null, 'release HTML never executes')
  assert.equal(doc.querySelector('.update-markdown a[href^="javascript:"]'), null, 'unsafe release links are removed')
  assert.equal(g.__unsafeUpdate, undefined)
  const badge = doc.querySelector('.update-badge')
  badge.focus()
  await click(badge)
  dialog = doc.querySelector('[role="dialog"]')
  assert.ok(dialog.querySelector('.update-install-notice'), 'installation consequences are explained before confirmation')
  assert.equal(posted, 0, 'opening either entry never installs')
  await click(dialog.querySelector('.update-view-version'))
  assert.equal(versionNavigations, 1)
  assert.equal(doc.querySelector('[role="dialog"]'), null, 'opening the version page closes the dialog first')
  console.log('ok   update badge uses the same dialog; release notes reuse safe Markdown; navigation does not install')

  uiCheck = { ...current, canInstall: false }
  await act(async () => { await mod.checkForUpdates(true); mod.openUpdateDialog(); await settle() })
  dialog = doc.querySelector('[role="dialog"]')
  assert.match(dialog.textContent, /请在主机上安装更新/)
  assert.equal([...dialog.querySelectorAll('button')].some((button) => button.textContent === '下载并安装'), false)
  await act(async () => { mod.closeUpdateDialog(); await settle() })
  uiCheck = { ...current, latest: { ...current.latest, installer: null } }
  await act(async () => { await mod.checkForUpdates(true); mod.openUpdateDialog(); await settle() })
  dialog = doc.querySelector('[role="dialog"]')
  assert.match(dialog.textContent, /安装包尚未就绪/)
  assert.equal(dialog.querySelector('.update-install-notice'), null)
  await act(async () => { mod.closeUpdateDialog(); await settle() })
  console.log('ok   remote and missing-installer states never offer automatic installation')

  uiCheck = current
  uiStatus = downloading
  await act(async () => { await mod.checkForUpdates(true); await mod.refreshUpdateStatus(); mod.openUpdateDialog(); await settle() })
  const progress = doc.querySelector('[role="dialog"] [role="progressbar"]')
  assert.equal(progress.getAttribute('aria-valuenow'), '25')
  assert.match(doc.querySelector('[role="dialog"] .update-download-labels').textContent, /512 B.*2 KB.*25%/)
  await act(async () => { mod.closeUpdateDialog(); await settle() })
  assert.equal(mod.getUpdateState().status.phase, 'downloading')
  assert.ok(doc.querySelector('.version-page [role="progressbar"]'), 'closing the dialog retains visible progress on the version page')
  uiStatus = { ...downloading, phase: 'launched' }
  await act(async () => { await mod.refreshUpdateStatus(); mod.openUpdateDialog(); await settle() })
  assert.match(doc.querySelector('[role="dialog"]').textContent, /安装器已启动/)
  assert.doesNotMatch(doc.querySelector('[role="dialog"]').textContent, /更新成功|安装成功/)
  await act(async () => { mod.closeUpdateDialog(); await settle() })
  mod.setLocale('en')
  await act(async () => { mod.openUpdateDialog(); renderUI(); await settle() })
  assert.match(doc.querySelector('[role="dialog"]').textContent, /The installer has started/)
  assert.match(doc.querySelector('.version-page-header').textContent, /Version & updates/)
  assert.equal(posted, 0, 'UI verification never sends install requests')
  console.log('ok   accessible byte progress survives navigation; installer handoff is not upgrade success; English copy is complete')
} finally {
  await act(async () => { reactRoot.unmount(); await settle() })
  g.dispatchEvent(new g.Event('pagehide'))
  globalThis.fetch = nativeFetch
  for (const [key, descriptor] of globals) {
    if (descriptor) Object.defineProperty(globalThis, key, descriptor)
    else delete globalThis[key]
  }
  delete globalThis.IS_REACT_ACT_ENVIRONMENT
  dom.window.close()
}
console.log('updates UI checks passed')
