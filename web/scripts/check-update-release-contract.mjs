import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const text = readFileSync(new URL('../../.github/workflows/desktop-release.yml', import.meta.url), 'utf8').replace(/\r\n/g, '\n')
const trigger = text.split('\non:\n')[1]?.split('\npermissions:')[0]
assert.ok(trigger)
assert.deepEqual([...trigger.matchAll(/^  ([a-z_]+):/gm)].map(match => match[1]), ['workflow_dispatch'])
assert.ok(text.indexOf('node .github/scripts/release-version.mjs') < text.indexOf('@tauri-apps/cli@2 build'), '版本一致性校验必须在构建前')
assert.ok(!text.includes('desktop-$env:GITHUB_RUN_NUMBER'), '不能生成无法与本地版本比较的 Release 标签')
console.log('Update release contract: manual-only publishing and pre-build version consistency guard checked.')
