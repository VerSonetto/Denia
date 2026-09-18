/**
 * 右侧面板纯函数的回归网。
 *   node scripts/check-side-pane.mjs
 *
 * 为什么值得单独测:标签的**关闭/重排/重开**是最容易写错又最难靠肉眼发现
 * 的一类逻辑 —— 关中间标签后焦点该落在哪、重开浏览器标签为什么要换 id、
 * 分桶是否真的隔离,这些在浏览器里点几下很难覆盖全,但纯函数一测就清楚。
 *
 * 与 `check-fold.mjs` 同一套做法:esbuild bundle 成单文件后 import。
 */
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'

const dir = mkdtempSync(join(tmpdir(), 'side-pane-'))
const file = join(dir, 'sidePane.mjs')
await build({
  entryPoints: [fileURLToPath(new URL('../src/sidePane.ts', import.meta.url))],
  outfile: file,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  logLevel: 'silent',
})
const mod = await import(pathToFileURL(file).href)

let failed = 0
function check(label, actual, expected) {
  const a = JSON.stringify(actual)
  const e = JSON.stringify(expected)
  if (a !== e) {
    failed++
    console.log(`FAIL ${label}\n  actual   ${a}\n  expected ${e}`)
  } else {
    console.log(`ok   ${label}`)
  }
}

/** 造一个标签(只填测试关心的字段)。 */
const tab = (id, type = 'terminal', extra = {}) => ({
  id,
  type,
  openedAt: 1000,
  ...extra,
})

const ids = (state) => state.tabs.map((t) => t.id)

/* ---------- upsertTab ---------- */

{
  const empty = mod.EMPTY_SIDE_PANE
  const one = mod.upsertTab(empty, tab('a'))
  check('upsert:空态插入后激活新标签', { ids: ids(one), active: one.activeTabId }, {
    ids: ['a'],
    active: 'a',
  })

  const two = mod.upsertTab(one, tab('b'))
  check('upsert:追加并激活', { ids: ids(two), active: two.activeTabId }, {
    ids: ['a', 'b'],
    active: 'b',
  })

  // 同 id 更新必须**原地替换**,不能挪到末尾:重开最近关闭的标签要回到原位。
  const updated = mod.upsertTab(two, tab('a', 'terminal', { title: 'shell' }), {
    activate: false,
  })
  check(
    'upsert:同 id 原地替换且 activate:false 不改焦点',
    { ids: ids(updated), active: updated.activeTabId, title: updated.tabs[0].title },
    { ids: ['a', 'b'], active: 'b', title: 'shell' },
  )
}

/* ---------- closeTab:焦点落点 ---------- */

{
  const state = { tabs: [tab('a'), tab('b'), tab('c')], activeTabId: 'b' }
  // 关中间 → 优先激活右边那个(与浏览器一致)。
  const closed = mod.closeTab(state, 'b')
  check('close:关中间标签后焦点前移到右邻', { ids: ids(closed), active: closed.activeTabId }, {
    ids: ['a', 'c'],
    active: 'c',
  })

  // 关最右 → 右边没有了,回退到左邻。
  const rightmost = mod.closeTab({ tabs: [tab('a'), tab('b')], activeTabId: 'b' }, 'b')
  check('close:关最右标签后焦点回退到左邻', rightmost.activeTabId, 'a')

  // 关非活跃标签 → 焦点不动。
  const other = mod.closeTab({ tabs: [tab('a'), tab('b')], activeTabId: 'a' }, 'b')
  check('close:关非活跃标签不改变焦点', other.activeTabId, 'a')

  // 关最后一个 → 空态。
  const last = mod.closeTab({ tabs: [tab('a')], activeTabId: 'a' }, 'a')
  check('close:关最后一个标签回到空态', { ids: ids(last), active: last.activeTabId }, {
    ids: [],
    active: '',
  })

  // 关不存在的 id → 原样返回(同一个引用,便于 React 跳过重渲染)。
  const same = mod.closeTab(state, 'zzz')
  check('close:关不存在的 id 返回同一引用', same === state, true)
}

/* ---------- closeOtherTabs / closeAllTabs ---------- */

{
  const state = { tabs: [tab('a'), tab('b'), tab('c')], activeTabId: 'b' }
  const kept = mod.closeOtherTabs(state, 'b')
  check('closeOthers:只留指定标签', { ids: ids(kept), active: kept.activeTabId }, {
    ids: ['b'],
    active: 'b',
  })
  check('closeOthers:指定 id 不存在则原样返回', mod.closeOtherTabs(state, 'zzz') === state, true)
  check('closeAll:清空', ids(mod.closeAllTabs()), [])
}

/* ---------- reorderTab ---------- */

{
  const state = { tabs: [tab('a'), tab('b'), tab('c')], activeTabId: 'a' }
  // 把 c 拖到最前(跨多个位置,不是相邻交换)。
  check('reorder:跨位拖拽', ids(mod.reorderTab(state, 'c', 'a')), ['c', 'a', 'b'])
  // 相邻交换。
  check('reorder:相邻交换', ids(mod.reorderTab(state, 'a', 'b')), ['b', 'a', 'c'])
  // 自己拖自己 → 同一引用。
  check('reorder:同 id 返回同一引用', mod.reorderTab(state, 'a', 'a') === state, true)
  check('reorder:未知 id 返回同一引用', mod.reorderTab(state, 'zzz', 'a') === state, true)
  // 重排不改焦点。
  check('reorder:不改变激活标签', mod.reorderTab(state, 'c', 'a').activeTabId, 'a')
}

/* ---------- pushClosed:历史上限与去重 ---------- */

{
  const history = mod.pushClosed([], [tab('a'), tab('b')], 100)
  check('closed:记录顺序为新记录在前', history.map((h) => h.tab.id), ['a', 'b'])
  check('closed:带上关闭时刻', history[0].closedAt, 100)

  // 重复关闭同一个 id:旧记录被挤掉,不留两条。
  const again = mod.pushClosed(history, [tab('a')], 200)
  check('closed:重复 id 去重', again.map((h) => h.tab.id), ['a', 'b'])
  check('closed:去重后时刻更新', again[0].closedAt, 200)

  // 超过上限截断(上限 8)。
  let many = []
  for (let i = 0; i < 12; i += 1) many = mod.pushClosed(many, [tab(`t${i}`)], i)
  check('closed:超出上限截断到 8 条', many.length, 8)
  check('closed:保留最新的那条', many[0].tab.id, 't11')

  check('closed:空输入返回原数组', mod.pushClosed(history, [], 300) === history, true)
}

/* ---------- reviveClosed:浏览器必须换 id ---------- */

{
  const terminal = tab('term-1', 'terminal', { title: 'pwsh' })
  const revived = mod.reviveClosed(terminal, 'term-1')
  check('revive:终端保留 id 与标题', revived, { id: 'term-1', type: 'terminal', openedAt: 1000, title: 'pwsh' })

  // 浏览器旧实例已销毁:复用 id 会认领到不存在的会话。
  const browser = tab('browser:old', 'browser', { title: '旧页面' })
  const fresh = mod.reviveClosed(browser, 'browser:new')
  check(
    'revive:浏览器换新 id 且清掉旧标题',
    { id: fresh.id, title: fresh.title, type: fresh.type },
    { id: 'browser:new', title: undefined, type: 'browser' },
  )
}

/* ---------- 查询辅助 ---------- */

{
  const state = { tabs: [tab('r', 'review'), tab('t1'), tab('t2')], activeTabId: 't1' }
  check('hasType:审查存在', mod.hasType(state, 'review'), true)
  check('hasType:浏览器不存在', mod.hasType(state, 'browser'), false)
  check('hasType:空态一律 false', mod.hasType(null, 'review'), false)
  check('tabsOfType:筛出两个终端', mod.tabsOfType(state, 'terminal').map((t) => t.id), ['t1', 't2'])
  check('activeTab:取到激活项', mod.activeTab(state)?.id, 't1')
  check('findTab:未知 id 返回 null', mod.findTab(state, 'zzz'), null)
  check('activeTab:空态返回 null', mod.activeTab(null), null)
}

/* ---------- scopeKey:分桶隔离 ---------- */

{
  check('scope:有会话用会话 id', mod.scopeKey('sess-1'), 'sess-1')
  check('scope:无会话落到 draft 桶', mod.scopeKey(null), mod.DRAFT_SCOPE)
  check('scope:draft 桶名稳定', mod.DRAFT_SCOPE, '__draft__')
}

/* ---------- 文件读取标签页 ---------- */

{
  const T = 5000
  // 首次打开:创建标签页并激活。
  const first = mod.openFileInTab(null, { path: 'src/main.rs' }, T)
  check(
    'file:首次打开创建标签页并激活',
    { ids: ids(first), active: first.activeTabId, files: first.tabs[0].files.map((f) => f.path), activeFile: first.tabs[0].activeFile },
    { ids: ['file'], active: 'file', files: ['src/main.rs'], activeFile: 'src/main.rs' },
  )
  check('file:短名从路径推得', first.tabs[0].files[0].name, 'main.rs')

  // 同一文件重复打开:复用条目,不重复添加,不新建标签页。
  const again = mod.openFileInTab(first, { path: 'src/main.rs' }, T + 1)
  check(
    'file:重复打开同一文件不新增条目',
    { count: again.tabs.length, files: again.tabs[0].files.length },
    { count: 1, files: 1 },
  )

  // 打开另一个文件:追加条目并切激活项。
  const second = mod.openFileInTab(again, { path: 'a/b/c.ts' }, T + 2)
  check(
    'file:不同文件追加条目并切激活',
    { files: second.tabs[0].files.map((f) => f.path), activeFile: second.tabs[0].activeFile },
    { files: ['src/main.rs', 'a/b/c.ts'], activeFile: 'a/b/c.ts' },
  )

  // 切回已打开的文件:只换激活项,顺序不变(不因重复点击而跳动)。
  const back = mod.openFileInTab(second, { path: 'src/main.rs' }, T + 3)
  check(
    'file:切回已打开文件保持顺序',
    { files: back.tabs[0].files.map((f) => f.path), activeFile: back.tabs[0].activeFile },
    { files: ['src/main.rs', 'a/b/c.ts'], activeFile: 'src/main.rs' },
  )

  // 空路径不建标签页。
  check('file:空路径不打开', mod.openFileInTab(null, { path: '  ' }, T).tabs.length, 0)

  // 激活不存在的条目:原样返回。
  check('file:激活不存在的条目不变', mod.activateFile(second, 'nope.ts'), second)
  check(
    'file:激活已有条目',
    mod.activateFile(second, 'src/main.rs').tabs[0].activeFile,
    'src/main.rs',
  )

  // 关条目:当前激活项顺位接上邻居(优先右边)。
  const closedActive = mod.closeFile(second, 'a/b/c.ts')
  check(
    'file:关掉激活条目后顺位接邻居',
    { files: closedActive.tabs[0].files.map((f) => f.path), activeFile: closedActive.tabs[0].activeFile },
    { files: ['src/main.rs'], activeFile: 'src/main.rs' },
  )

  // 关掉非激活条目:激活项不受影响。
  const closedOther = mod.closeFile(second, 'src/main.rs')
  check(
    'file:关掉非激活条目不影响激活项',
    { files: closedOther.tabs[0].files.map((f) => f.path), activeFile: closedOther.tabs[0].activeFile },
    { files: ['a/b/c.ts'], activeFile: 'a/b/c.ts' },
  )

  // 关掉最后一个条目:标签页一并消失(它没有手动入口,空壳无意义)。
  const last = mod.closeFile(mod.openFileInTab(null, { path: 'only.ts' }, T), 'only.ts')
  check('file:关掉最后一个条目连带关掉标签页', { ids: ids(last), active: last.activeTabId }, { ids: [], active: '' })

  // 关掉标签页后重新打开:能重新创建(要求里明确的一条)。
  const reopened = mod.openFileInTab(last, { path: 'fresh.ts' }, T + 4)
  check(
    'file:关闭后可重新创建',
    { ids: ids(reopened), files: reopened.tabs[0].files.map((f) => f.path) },
    { ids: ['file'], files: ['fresh.ts'] },
  )

  // 标签页已存在时打开文件:不能新建第二个标签页。
  check(
    'file:已存在时复用同一标签页',
    mod.openFileInTab(reopened, { path: 'x.ts' }, T + 5).tabs.length,
    1,
  )

  // 显式传 name 时用它,不从路径推。
  check(
    'file:显式 name 优先',
    mod.openFileInTab(null, { path: 'a/b/c.ts', name: '自定义' }, T).tabs[0].files[0].name,
    '自定义',
  )

  // fileTab 查询。
  check('file:fileTab 取到标签页', mod.fileTab(first)?.id, 'file')
  check('file:无标签页时 fileTab 返回 null', mod.fileTab(mod.EMPTY_SIDE_PANE), null)
  check('file:basename 兼容反斜杠', mod.fileBasename('a\\b\\c.ts'), 'c.ts')
}

/* ---------- 分桶上限:putScope 的 LRU 语义 ---------- */

{
  // putScope 是 store 内部函数,不直接导出;这里用公开动作间接验证
  // "空标签不建桶"这一条(它是上限不被撑爆的前提)。
  const state = mod.upsertTab(mod.EMPTY_SIDE_PANE, tab('a'))
  const back = mod.closeTab(state, 'a')
  check('桶:关掉最后一个标签回到空态(调用方据此删桶)', ids(back), [])
}

/* ---------- 汇总 ---------- */
console.log('')
if (failed > 0) {
  console.log(`✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('✓ 全部通过')
