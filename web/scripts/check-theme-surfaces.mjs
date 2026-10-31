import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'

const foundation = readFileSync(new URL('../src/styles/foundation.css', import.meta.url), 'utf8')
const settings = readFileSync(new URL('../src/styles/settings.css', import.meta.url), 'utf8')
const markdown = readFileSync(new URL('../src/markdown.css', import.meta.url), 'utf8')
const systemLight = foundation.match(/@media \(prefers-color-scheme: light\)\s*\{\s*:root:not\(\[data-theme='dark'\]\)\s*\{([\s\S]*?)color-scheme: light;/)?.[1]
const explicitLight = foundation.match(/:root\[data-theme='light'\]\s*\{([\s\S]*?)color-scheme: light;/)?.[1]
for (const theme of [systemLight, explicitLight]) {
  assert.ok(theme, 'light theme definition must exist')
  assert.match(theme, /--bg-layer-2:\s*#faf9f6\s*;/)
  assert.match(theme, /--bg-layer-3:\s*#f3f2ee\s*;/)
  assert.match(theme, /--code-block:\s*var\(--bg-layer-2\)/)
  assert.match(theme, /--bubble-fill:\s*var\(--bg-layer-3\)/)
}
assert.doesNotMatch(settings, /--bg-layer-[23]\s*:/, 'settings must inherit global surfaces')
assert.doesNotMatch(markdown, /--md-code-banner:\s*#f5f5f5/i)
console.log('ok light theme surfaces: explicit and system themes share warm global tokens')
