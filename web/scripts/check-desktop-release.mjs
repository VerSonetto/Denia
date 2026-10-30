import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const text = readFileSync(new URL('../../.github/workflows/desktop-release.yml', import.meta.url), 'utf8').replace(/\r\n/g, '\n')
const trigger = text.split('\non:\n')[1]?.split('\npermissions:')[0]
assert.ok(trigger)
assert.deepEqual([...trigger.matchAll(/^  ([a-z_]+):/gm)].map(match => match[1]), ['workflow_dispatch'])
// 前端产物在编译期被 rust-embed 嵌入,必须先于桌面端构建完成。
assert.ok(text.indexOf('vite build') < text.indexOf('@tauri-apps/cli@2 build'))
assert.ok(text.includes('--target x86_64-pc-windows-msvc'))
assert.ok(text.includes('target/x86_64-pc-windows-msvc/release/denia-desktop.exe'))
assert.ok(text.includes('--frozen-lockfile'))
assert.ok(text.includes("'--target', $env:GITHUB_SHA"))
assert.ok(text.includes('needs: windows'))
assert.ok(text.includes('contents: write'))
assert.ok(text.includes('SHA256SUMS.txt'))
assert.ok(text.includes('--bundles nsis'), 'installer build is required')
assert.ok(text.includes('tauri.release.conf.json'))
assert.ok(!text.includes('Compress-Archive'), 'no compressed archive is published')
assert.ok(text.includes('release/*.exe'))
assert.ok(text.includes('$files.Count -ne 3'), 'standalone EXE, installer and checksums')
const overlay = JSON.parse(readFileSync(new URL('../../crates/desktop/tauri.release.conf.json', import.meta.url), 'utf8'))
assert.deepEqual(overlay.bundle.targets, ['nsis'])
assert.equal(overlay.bundle.active, true)
assert.ok(overlay.bundle.windows.webviewInstallMode.type)
assert.ok(!text.includes('--clobber'))
assert.ok(!text.includes('${{ inputs.tag }}', text.indexOf('run: |')), 'Tag must be passed through environment, not shell interpolation')
console.log('PASS manual desktop release trigger, locked builds, artifact paths and safe publication contract')
