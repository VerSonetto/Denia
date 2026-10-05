// One test inventory for development, CI and production builds. Report every
// failing suite rather than stopping before unrelated suites have been checked.
import { readdirSync } from 'node:fs'
import { spawnSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'

const directory = fileURLToPath(new URL('.', import.meta.url))
const memorySuites = new Set([
  'check-session-memory.mjs', 'check-viewport-memory.mjs', 'check-session-view-memory.mjs',
])
const memoryOnly = process.argv.includes('--memory')
const suites = readdirSync(directory).filter(name =>
  /^check-.+\.mjs$/.test(name) && (!memoryOnly || memorySuites.has(name)),
).sort()
const failed = []
for (const suite of suites) {
  process.stdout.write(`\n${suite}\n`)
  const flags = suite === 'check-viewport-memory.mjs' ? ['--expose-gc'] : []
  const result = spawnSync(process.execPath, [...flags, `${directory}${suite}`], {stdio: 'inherit'})
  if (result.error || result.status !== 0) failed.push(suite)
}
if (failed.length) {
  process.stderr.write(`\nFailed suites: ${failed.join(', ')}\n`)
  process.exitCode = 1
}
