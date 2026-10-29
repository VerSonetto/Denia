// Provider route edits: renaming must be a single atomic map move (old key
// removed and new key written together), not a two-step create/delete.
import assert from 'node:assert/strict'
import { build } from 'esbuild'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const root = fileURLToPath(new URL('..', import.meta.url))
const dir = mkdtempSync(join(tmpdir(), 'denia-provider-rename-'))
try {
  const bundle = join(dir, 'providerRename.mjs')
  await build({
    entryPoints: [join(root, 'src/components/llm/providerRename.ts')],
    outfile: bundle,
    bundle: true,
    format: 'esm',
    platform: 'node',
    logLevel: 'silent',
  })
  const { applyRouteChange, isRename } = await import(pathToFileURL(bundle).href)

  const profile = { baseURL: 'https://old.example/v1', protocol: 'openai-completions' }
  const nextProfile = { ...profile, displayName: '改名后' }
  const base = {
    alpha: profile,
    beta: { baseURL: 'https://beta.example/v1' },
  }

  const added = applyRouteChange(base, { route: 'gamma', profile })
  assert.deepEqual(Object.keys(added), ['alpha', 'beta', 'gamma'], '新路由只增加一个键')
  assert.equal(added.gamma, profile)

  const edited = applyRouteChange(base, { route: 'alpha', profile: nextProfile })
  assert.equal(edited.alpha, nextProfile, '编辑沿用原键')
  assert.deepEqual(Object.keys(edited), ['alpha', 'beta'], '编辑不新增键')

  const renamed = applyRouteChange(base, {
    route: 'alpha-2',
    previousRoute: 'alpha',
    profile: nextProfile,
  })
  assert.deepEqual(Object.keys(renamed), ['beta', 'alpha-2'], '改名同时删旧键、写新键')
  assert.equal(renamed.alpha, undefined, '旧 id 不残留')
  assert.equal(renamed['alpha-2'], nextProfile, 'profile 跟着迁移')
  assert(isRename({ route: 'alpha-2', previousRoute: 'alpha', profile }), '识别改名')

  // 同一个键不算改名:即使调用方传了 previousRoute,也不能把刚写入的键删掉。
  const same = applyRouteChange(base, {
    route: 'alpha',
    previousRoute: 'alpha',
    profile: nextProfile,
  })
  assert.equal(same.alpha, nextProfile, '同键迁移不删除新内容')
  assert(!isRename({ route: 'alpha', previousRoute: 'alpha', profile }), '同键不是改名')
  assert.deepEqual(Object.keys(base), ['alpha', 'beta'], '不修改入参键集合')
  assert.equal(base.alpha, profile, '不替换入参里的 profile')

  const removed = applyRouteChange(base, { route: 'alpha', profile: null })
  assert.deepEqual(Object.keys(removed), ['beta'], '删除只移除目标键')

  console.log('ok provider id rename is one atomic map move')
} finally {
  rmSync(dir, { recursive: true, force: true })
}
