import assert from 'node:assert/strict'
import { build } from 'esbuild'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
const root = fileURLToPath(new URL('..', import.meta.url))
const dir = mkdtempSync(join(tmpdir(), 'denia-sidebar-order-'))
try {
  const output = join(dir, 'order.mjs')
  await build({ entryPoints: [join(root, 'src/sidebarOrder.ts')], outfile: output, bundle: true, format: 'esm', platform: 'node', logLevel: 'silent' })
  const { applyManualOrder, moveId, applyPins, toggleId } = await import(pathToFileURL(output).href)

  const items = [{ id: 'a' }, { id: 'b' }, { id: 'c' }]
  assert.deepEqual(applyManualOrder(items, undefined), items, '没有手动顺序时保持服务端顺序')
  assert.deepEqual(applyManualOrder(items, []), items)
  assert.deepEqual(applyManualOrder(items, ['c', 'a', 'b']).map((item) => item.id), ['c', 'a', 'b'])
  assert.deepEqual(applyManualOrder(items, ['b']).map((item) => item.id), ['a', 'c', 'b'], '新出现的项排在顶部')
  assert.deepEqual(applyManualOrder(items, ['gone', 'c', 'c']).map((item) => item.id), ['a', 'b', 'c'], '失效与重复 id 忽略')

  assert.equal(applyPins(items, []), items, '没有置顶时保持原数组')
  assert.deepEqual(applyPins(items, ['c']).map((item) => item.id), ['c', 'a', 'b'], '置顶项稳定上浮')
  assert.deepEqual(applyPins(items, ['c', 'a']).map((item) => item.id), ['a', 'c', 'b'], '多个置顶项保留原相对顺序')
  assert.equal(applyPins(items, ['gone']), items, '失效置顶 id 不改变顺序')
  assert.deepEqual(toggleId(['a', 'b'], 'b'), ['a'], '再次置顶即取消')
  assert.deepEqual(toggleId(['a'], 'b'), ['a', 'b'], '新置顶追加到末尾')
  assert.deepEqual(toggleId([], 'x'), ['x'])

  const ids = ['a', 'b', 'c']
  assert.equal(moveId(ids, 'a', 'a', 'after'), ids, '原地拖动不产生新数组')
  assert.equal(moveId(ids, 'x', 'a', 'before'), ids, '未知源 id 不改变顺序')
  assert.equal(moveId(ids, 'a', 'x', 'after'), ids, '未知目标 id 不改变顺序')
  assert.equal(moveId(ids, 'a', 'b', 'before'), ids, '结果未变化时返回同一引用')
  assert.deepEqual(moveId(ids, 'a', 'c', 'after'), ['b', 'c', 'a'], '向下拖到目标之后')
  assert.deepEqual(moveId(ids, 'c', 'a', 'before'), ['c', 'a', 'b'], '向上拖到目标之前')
  assert.deepEqual(moveId(ids, 'c', 'b', 'before'), ['a', 'c', 'b'])
  assert.deepEqual(ids, ['a', 'b', 'c'], '不改写传入数组')
  console.log('ok sidebar manual order merge and move semantics')
} finally { rmSync(dir, { recursive: true, force: true }) }
