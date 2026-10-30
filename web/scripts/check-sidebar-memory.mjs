import assert from 'node:assert/strict'
import { build } from 'esbuild'
import { mkdtempSync, rmSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
const root = fileURLToPath(new URL('..', import.meta.url))
const dir = mkdtempSync(join(tmpdir(), 'denia-sidebar-memory-'))
try {
  const output = join(dir, 'memory.mjs')
  await build({ entryPoints: [join(root, 'src/sidebarMemory.ts')], outfile: output, bundle: true, format: 'esm', platform: 'node', logLevel: 'silent' })
  const store = new Map()
  globalThis.window = {
    localStorage: {
      getItem: (key) => store.get(key) ?? null,
      setItem: (key, value) => { store.set(key, String(value)) },
      removeItem: (key) => { store.delete(key) },
    },
    location: { hash: '#s=child%201' },
  }
  const {
    readCollapsedChildren, writeCollapsedChildren, restoredSidebarSession,
    readSidebarOrder, writeSidebarOrder, readSidebarFilter, writeSidebarFilter,
    readSidebarPins, writeSidebarPins,
    readInProgressMarks, writeInProgressMarks,
  } = await import(pathToFileURL(output).href)

  assert.deepEqual(readCollapsedChildren(), {})
  writeCollapsedChildren({ parent: true, expanded: false })
  assert.deepEqual(readCollapsedChildren(), { parent: true }, '刷新后保留收起状态')
  writeCollapsedChildren({})
  assert.deepEqual(readCollapsedChildren(), {}, '全部展开清除收起记忆')
  store.set('denia.sidebar-collapsed-children.v1', '{bad')
  assert.deepEqual(readCollapsedChildren(), {})
  store.set('denia.sidebar-collapsed-children.v1', '[true]')
  assert.deepEqual(readCollapsedChildren(), {})
  store.set('denia.sidebar-collapsed-children.v1', '{"a":true,"b":"true","c":false}')
  assert.deepEqual(readCollapsedChildren(), { a: true })
  assert.equal(restoredSidebarSession(null), 'child 1', '初始化识别待恢复会话，不触发祖先展开')
  assert.equal(restoredSidebarSession('current'), 'current')
  window.location.hash = '#s=%'
  assert.equal(restoredSidebarSession(null), null)

  assert.deepEqual(readSidebarOrder(), { workspaces: [], ungrouped: [], sessions: {} }, '默认无手动顺序')
  writeSidebarOrder({ workspaces: ['w2', 'w1'], ungrouped: ['u1'], sessions: { ws: ['s2', 's1'], empty: [] } })
  assert.deepEqual(readSidebarOrder(), { workspaces: ['w2', 'w1'], ungrouped: ['u1'], sessions: { ws: ['s2', 's1'] } }, '排序账本可往返，空分组不落盘')
  store.set('denia.sidebar-order.v1', '{bad')
  assert.deepEqual(readSidebarOrder(), { workspaces: [], ungrouped: [], sessions: {} })
  store.set('denia.sidebar-order.v1', '[true]')
  assert.deepEqual(readSidebarOrder(), { workspaces: [], ungrouped: [], sessions: {} })
  store.set('denia.sidebar-order.v1', '{"workspaces":["a",1,"","b"],"ungrouped":"x","sessions":{"ws":["s",2],"bad":"x","empty":[]}}')
  assert.deepEqual(readSidebarOrder(), { workspaces: ['a', 'b'], ungrouped: [], sessions: { ws: ['s'] } }, '非字符串 id 与非法分组成丢弃')

  assert.equal(readSidebarFilter(), 'all', '默认筛选全部')
  writeSidebarFilter('running')
  assert.equal(readSidebarFilter(), 'running')
  writeSidebarFilter('all')
  assert.equal(store.has('denia.sidebar-filter.v1'), false, '回到全部时清除筛选记忆')
  store.set('denia.sidebar-filter.v1', 'weird')
  assert.equal(readSidebarFilter(), 'all', '未知值回落全部')

  assert.deepEqual(readSidebarPins(), { workspaces: [], sessions: [] }, '默认无置顶')
  writeSidebarPins({ workspaces: ['w2', 'w1'], sessions: ['s1'] })
  assert.deepEqual(readSidebarPins(), { workspaces: ['w2', 'w1'], sessions: ['s1'] }, '置顶账本可往返')
  store.set('denia.sidebar-pins.v1', '{bad')
  assert.deepEqual(readSidebarPins(), { workspaces: [], sessions: [] })
  store.set('denia.sidebar-pins.v1', '{"workspaces":["w",1,""],"sessions":"x"}')
  assert.deepEqual(readSidebarPins(), { workspaces: ['w'], sessions: [] }, '非法置顶项丢弃')

  assert.deepEqual(readInProgressMarks(), [], '默认没有进行中标记')
  writeInProgressMarks(['s1', 's1', '', 's2'])
  assert.deepEqual(readInProgressMarks(), ['s1', 's2'], '标记去重且忽略空 id')
  writeInProgressMarks([])
  assert.equal(store.has('denia.sidebar-in-progress.v1'), false, '清空标记时移除键')
  store.set('denia.sidebar-in-progress.v1', '{bad')
  assert.deepEqual(readInProgressMarks(), [])
  store.set('denia.sidebar-in-progress.v1', '["a",2,"b","a"]')
  assert.deepEqual(readInProgressMarks(), ['a', 'b'], '非字符串标记丢弃且读取去重')

  window.localStorage.getItem = () => { throw new Error('blocked') }
  window.localStorage.setItem = () => { throw new Error('quota') }
  window.localStorage.removeItem = () => { throw new Error('blocked') }
  assert.deepEqual(readCollapsedChildren(), {})
  assert.deepEqual(readSidebarOrder(), { workspaces: [], ungrouped: [], sessions: {} })
  assert.deepEqual(readSidebarPins(), { workspaces: [], sessions: [] })
  assert.equal(readSidebarFilter(), 'all')
  assert.deepEqual(readInProgressMarks(), [])
  assert.doesNotThrow(() => writeCollapsedChildren({ parent: true }))
  assert.doesNotThrow(() => writeSidebarOrder({ workspaces: ['w'], ungrouped: [], sessions: {} }))
  assert.doesNotThrow(() => writeSidebarPins({ workspaces: ['w'], sessions: ['s'] }))
  assert.doesNotThrow(() => writeSidebarFilter('running'))
  assert.doesNotThrow(() => writeInProgressMarks(['s']))
  // 侧栏工作区树已整体抽到 components/sidebar;以下断言跟随迁移落在新文件。
  const app = readFileSync(join(root, 'src/components/sidebar/SidebarWorkspaces.tsx'), 'utf8')
  assert.ok(app.includes('useState(readCollapsedChildren)'))
  assert.ok(app.includes('activeId === revealedSessionRef.current'), '同会话推送不覆盖折叠状态')
  assert.ok(app.includes('beginSessionDrag'), '侧栏会话支持拖动排序')
  assert.ok(app.includes('beginWorkspaceDrag'), '侧栏工作区支持拖动排序')
  assert.ok(app.includes("t('sidebarFilterRunning')"), '侧栏提供进行中筛选标签')
  assert.ok(app.includes('markInProgress'), '会话右键菜单提供标记进行中')
  assert.ok(app.includes('unmarkInProgress'), '会话右键菜单提供移除标记')
  assert.ok(app.includes('inProgress.has(session.id)'), '进行中筛选按手动标记与 running 的并集')
  assert.ok(app.includes('readInProgressMarks'), '标记跨重启恢复')
  assert.ok(app.includes('writeInProgressMarks'), '标记持久化到本机账本')
  assert.ok(app.includes('sessionsLoaded'), '列表加载完成前不清理标记')
  assert.ok(app.includes('sawSessionsRef'), '首次空列表不清标记,见过真实列表后空列表才权威')
  assert.equal(
    app.split('marked={inProgress.has(session.id)}').length - 1,
    2,
    '分组与未分组会话都传入标记',
  )
  const row = readFileSync(join(root, 'src/components/sidebar/SessionRow.tsx'), 'utf8')
  assert.ok(row.includes('data-testid="in-progress-mark"'), '标记有稳定的测试锚点')
  assert.ok(row.includes("t('markedInProgressHint')"), '标记有可访问的提示文案')
  assert.ok(app.includes('openSessionMenu'), '会话支持自定义右键菜单')
  assert.ok(app.includes('openWorkspaceMenu'), '工作区支持自定义右键菜单')
  assert.ok(app.includes("t('confirmDeleteInline')"), '删除改为行内二次确认')
  assert.ok(app.includes('togglePinned'), '工作区与会话支持置顶')
  const main = readFileSync(join(root, 'src/main.tsx'), 'utf8')
  assert.ok(main.includes("addEventListener('contextmenu'"), '全局禁用浏览器默认右键菜单')
  console.log('ok sidebar collapse persistence, manual order, task filter and storage failures')
} finally { rmSync(dir, { recursive: true, force: true }) }
