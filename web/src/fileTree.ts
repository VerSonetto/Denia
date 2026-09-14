/**
 * 工作区文件树的纯函数核心(零 React/DOM 依赖,便于单测)。
 *
 * # 为什么是「逐层懒加载」而不是一次拉全树
 *
 * @ 提及候选走的是"全树索引 + 模糊匹配"(见后端 `fs.rs` 的 `mentions_impl`),
 * 它必须一次给出任意深度的命中,所以愿意付全树扫描的代价。文件树相反:
 * 用户是**顺着目录往下看**,一次只需要一层的正确内容,而大仓库整树灌进
 * 浏览器既慢又占内存。这里因此按目录懒加载,展开哪层读哪层。
 *
 * # 状态形状
 *
 * 节点用**相对根的路径**作键(根是空串 `''`),与后端、@ 提及、拖拽载荷
 * 共用同一套坐标 —— 三处路径形态一旦分叉,就会出现"拖过去引用不到"的
 * 一类难查 bug。每个节点记 `children`(null = 还没拉过)、`expanded`、
 * `loading`、`error`。
 *
 * `visibleRows()` 把树展平成行列表:它是唯一决定"画什么"的函数,
 * 折叠语义(收起时子行全部隐藏、祖先收起则后代不可见)因此可以直接断言。
 */

import { formatFileMention } from './pages/mention'

/** 一个已加载的条目(来自后端一层列举)。 */
export interface TreeEntry {
  name: string
  /** 相对根的路径,以 `/` 分隔。 */
  path: string
  kind: 'file' | 'directory'
}

/** 单个目录节点的状态。文件不建节点(它们没有可展开的内容)。 */
export interface TreeDirState {
  /** 已加载的子项路径(顺序即后端给的顺序:目录优先 + 字典序)。 */
  children: string[] | null
  /** 该层条目超限被截断。 */
  truncated: boolean
  expanded: boolean
  loading: boolean
  /** 列举失败的原因(展开时读盘出错)。 */
  error: string
}

/** 整棵树的状态:根路径 + 以相对路径为键的目录节点表。 */
export interface FileTreeState {
  root: string
  dirs: Record<string, TreeDirState>
  /** 条目表:相对路径 → 条目(文件与目录都在这里)。 */
  entries: Record<string, TreeEntry>
}

export function emptyTree(root: string): FileTreeState {
  return { root, dirs: {}, entries: {} }
}

function dirState(state: FileTreeState, path: string): TreeDirState {
  return (
    state.dirs[path] ?? {
      children: null,
      truncated: false,
      expanded: false,
      loading: false,
      error: '',
    }
  )
}

/** 标记某目录正在加载(展开动作发起时调用)。 */
export function markLoading(state: FileTreeState, path: string): FileTreeState {
  return {
    ...state,
    dirs: { ...state.dirs, [path]: { ...dirState(state, path), loading: true, error: '' } },
  }
}

/**
 * 并入一层列举结果。
 *
 * 覆盖该目录已有的 children(刷新语义:重新列举后以服务端为准),
 * 同时把条目并入 entries 表。已存在节点的展开态**保留** —— 刷新根目录
 * 不该把用户展开的子树折叠回去(用户视角是"内容更新了",不是"树被重置")。
 */
export function withListing(
  state: FileTreeState,
  dir: string,
  entries: TreeEntry[],
  truncated: boolean,
): FileTreeState {
  const nextEntries = { ...state.entries }
  const childPaths: string[] = []
  for (const entry of entries) {
    nextEntries[entry.path] = entry
    childPaths.push(entry.path)
  }
  const previous = dirState(state, dir)
  // 被覆盖的旧子项若已从磁盘消失,连同它们的节点一起清掉(否则展开态会
  // 挂在一个不存在的路径上,再展开时读盘报错)。
  const stale = (previous.children ?? []).filter((path) => !nextEntries[path])
  let dirs = { ...state.dirs }
  for (const path of stale) {
    if (dirs[path]) {
      dirs = { ...dirs }
      delete dirs[path]
    }
  }
  dirs[dir] = {
    ...previous,
    children: childPaths,
    truncated,
    loading: false,
    error: '',
  }
  return { ...state, dirs, entries: nextEntries }
}

/** 记录某目录列举失败。 */
export function withError(state: FileTreeState, dir: string, message: string): FileTreeState {
  return {
    ...state,
    dirs: {
      ...state.dirs,
      [dir]: { ...dirState(state, dir), loading: false, error: message, expanded: true },
    },
  }
}

/**
 * 展开/收起一个目录,并告知调用方"是否需要去拉这一层"。
 *
 * 只在"要展开且还没加载过"时返回 needLoad —— 重复展开用缓存,不重复读盘。
 */
export function toggleDir(
  state: FileTreeState,
  path: string,
): { state: FileTreeState; needLoad: boolean } {
  const current = dirState(state, path)
  const expanded = !current.expanded
  const needLoad = expanded && current.children === null && !current.loading
  return {
    state: {
      ...state,
      dirs: {
        ...state.dirs,
        [path]: {
          ...current,
          expanded,
          // 上次失败后再展开:重试一次(needLoad 同样为 true)。
          loading: needLoad ? true : current.loading,
          error: needLoad ? '' : current.error,
        },
      },
    },
    needLoad,
  }
}

/**
 * 刷新:重新从根拉一遍,但**保留已展开目录的展开态**。
 *
 * 保留展开态是有意的:用户点刷新是因为"磁盘变了",而不是"我想把树收起
 * 重来"。代价是这些目录的 children 要清空重拉(内容可能已变),所以调用方
 * 拿 `expandedDirs()` 补拉它们;条目表一并清空(旧条目可能已从磁盘消失)。
 */
export function resetTree(state: FileTreeState): FileTreeState {
  const dirs: Record<string, TreeDirState> = {}
  for (const [path, node] of Object.entries(state.dirs)) {
    // 根节点不是可见行(它的 expanded 没有意义),只要保住节点本身。
    if (path === '' || node.expanded) {
      dirs[path] = {
        ...node,
        children: null,
        truncated: false,
        loading: false,
        error: '',
      }
    }
  }
  if (!dirs['']) {
    dirs[''] = { children: null, truncated: false, expanded: false, loading: false, error: '' }
  }
  return { root: state.root, entries: {}, dirs }
}

/** 当前处于展开态的子目录(刷新后据此补拉,顺序稳定:浅层优先)。 */
export function expandedDirs(state: FileTreeState): string[] {
  return Object.entries(state.dirs)
    .filter(([path, node]) => path !== '' && node.expanded)
    .map(([path]) => path)
    .sort((left, right) => left.split('/').length - right.split('/').length || left.localeCompare(right))
}

/** 展平后的一行(缩进深度用于渲染)。 */
export interface TreeRow {
  entry: TreeEntry
  depth: number
  /** 该行目录当前是否展开(渲染 caret 朝向)。 */
  expanded: boolean
  /** 目录正在加载子项。 */
  loading: boolean
  /** 目录子项超限被截断(在该目录的最后一行下提示)。 */
  truncated: boolean
}

/**
 * 把树展平成可见行。
 *
 * 规则:从根的子项开始;遇到**已展开**的目录,把它的子项紧跟其后递归插入。
 * 未加载(children === null)的展开目录没有子行 —— 调用方负责在展开时触发加载。
 */
export function visibleRows(state: FileTreeState): TreeRow[] {
  const rows: TreeRow[] = []
  const walk = (parent: string, depth: number) => {
    const children = dirState(state, parent).children
    if (!children) return
    for (const path of children) {
      const entry = state.entries[path]
      if (!entry) continue
      const node = state.dirs[path]
      const expanded = entry.kind === 'directory' && node?.expanded === true
      rows.push({
        entry,
        depth,
        expanded,
        loading: node?.loading === true,
        truncated: expanded && node?.truncated === true,
      })
      if (expanded) walk(path, depth + 1)
    }
  }
  walk('', 0)
  return rows
}

/** 某目录当前的错误信息(没有则空串)。 */
export function dirError(state: FileTreeState, path: string): string {
  return state.dirs[path]?.error ?? ''
}

/* ---- 拖拽载荷 ---- */

/**
 * 拖拽引用的自定义 MIME。
 *
 * 为什么不用 `text/plain`:输入框已经要接受从**外部**(编辑器、文件管理器)
 * 拖进来的普通文本,两条来源必须能区分开,否则从别处拖一段文字进来会被
 * 误当成文件引用插成 `@...`。自定义 MIME 让"是不是树里拖来的"一目了然。
 */
export const REFERENCE_MIME = 'application/x-denia-reference'

/** 拖拽载荷:相对根的路径 + 类型(目录插入时要带尾 `/`)。 */
export interface ReferencePayload {
  path: string
  kind: 'file' | 'directory'
}

export function encodeReference(payload: ReferencePayload): string {
  return JSON.stringify({ path: payload.path, kind: payload.kind })
}

/** 解析拖拽载荷;形状不对返回 null(不抛:拖拽数据来自浏览器,可能是垃圾)。 */
export function decodeReference(raw: string): ReferencePayload | null {
  try {
    const value: unknown = JSON.parse(raw)
    if (!value || typeof value !== 'object') return null
    const candidate = value as Partial<ReferencePayload>
    if (typeof candidate.path !== 'string' || candidate.path.length === 0) return null
    if (candidate.kind !== 'file' && candidate.kind !== 'directory') return null
    return { path: candidate.path, kind: candidate.kind }
  } catch {
    return null
  }
}

/**
 * 引用插入文本:与 @ 提及**共用同一套格式化规则**,所以直接委托
 * `formatFileMention` 而不是复制一份 —— 拖拽插入与手打 `@` 选中的结果必须
 * 一模一样,否则模型侧会看到两种引用语法变体。无法安全表示的路径返回 ''。
 *
 * `preserveQuote` 传 false:拖拽没有"当前是否在引号里"的上下文,由路径自身
 * 是否含空白决定引号形式。
 */
export function referenceText(payload: ReferencePayload): string {
  return formatFileMention({ path: payload.path, kind: payload.kind }, false) ?? ''
}

/**
 * 把引用文本插到草稿的指定偏移处(拖拽落点)。
 *
 * 与 `applyMentionInsertion` 的差别只有一处:那个是替换光标处的 `@token`,
 * 这个是**在任意落点插入**(拖拽时没有 token 可替换)。空白补全规则一致。
 */
export function insertReferenceAt(
  draft: string,
  caret: number,
  reference: string,
): { text: string; caret: number } {
  const at = Math.max(0, Math.min(caret, draft.length))
  const before = draft.slice(0, at)
  const after = draft.slice(at)
  // 前:紧贴非空白字符时补一个空格,避免引用与相邻文字粘成一个 token。
  const lead = before.length === 0 || /\s$/u.test(before) ? '' : ' '
  // 后:同理;草稿末尾也补一个(光标落到引用之后,继续输入不会粘上)。
  // 这一条与 `applyMentionInsertion` 的 needsSpace 规则保持一致。
  const trail = after.length === 0 || !/^\s/u.test(after) ? ' ' : ''
  const inserted = `${lead}${reference}${trail}`
  return { text: `${before}${inserted}${after}`, caret: at + inserted.length }
}
