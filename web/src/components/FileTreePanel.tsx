/**
 * 工作区文件面板:相对工作区根的文件系统目录树。
 *
 * # 与「审查」面板的分工
 *
 * 审查面板看的是 **Git 工作区**(暂存/未暂存/分支差异),只显示被 git 跟踪
 * 或有改动的文件;本面板看的是**文件系统本身** —— 未跟踪的、被 .gitignore
 * 排除的、甚至不在仓库里的目录都照常列出。两者解决的是不同问题,所以不做
 * 任何互相过滤。
 *
 * # 三条关键行为
 *
 * 1. **逐层懒加载**。展开哪层读哪层(见 `fileTree.ts` 的模块注释),
 *    大仓库首屏只读一次根目录。
 * 2. **刷新不折叠**。刷新重新拉根目录,但保留用户已展开的子树展开态;
 *    已从磁盘消失的子项连同节点一起清掉(否则展开态挂在不存在的路径上)。
 * 3. **拖拽引用**。每行可拖到对话输入框,落下即插入 `@路径` 引用,与手打
 *    `@` 选中候选的结果完全一致(共用 `formatFileMention`)。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { t } from '../i18n'
import { fetchTree } from '../api'
import {
  REFERENCE_MIME,
  dirError,
  emptyTree,
  encodeReference,
  expandedDirs,
  markLoading,
  resetTree,
  toggleDir,
  visibleRows,
  withError,
  withListing,
  type FileTreeState,
  type TreeRow,
} from '../fileTree'
import { IconCaretRight, IconFile, IconFolder, IconRefresh, IconSpinner } from './icons'
import './FileTreePanel.css'

export interface FileTreePanelProps {
  /** 工作区绝对路径(树的根)。 */
  workspacePath: string
  /** 会话 id:切会话时重新从根加载(不同会话可能绑定不同工作区)。 */
  sessionId: string | null
  /**
   * 点击文件行:在「文件读取」标签页里打开该文件。
   *
   * 目录行不走这里 —— 点击目录仍然是展开/收起(那是树自己的主交互)。
   */
  onOpenFile?: (path: string) => void
}

export function FileTreePanel({ workspacePath, sessionId, onOpenFile }: FileTreePanelProps) {
  const [tree, setTree] = useState<FileTreeState>(() => emptyTree(workspacePath))
  const [filter, setFilter] = useState('')
  /**
   * 每个目录一个请求代号:防止慢响应覆盖新状态。
   *
   * 必须**按目录**记而不是全局一个计数器 —— 刷新会同时发起根目录与多个已
   * 展开目录的请求,全局计数器会让后发的把先发的判成"过期"而丢弃,表现是
   * 刷新后大部分目录一直停在加载态。
   */
  const revisionsRef = useRef<Map<string, number>>(new Map())
  const aliveRef = useRef(true)
  /** 当前树状态的最新值:事件回调里要用它算"哪些目录已展开"(见 refresh)。 */
  const treeRef = useRef(tree)
  treeRef.current = tree

  useEffect(() => {
    aliveRef.current = true
    return () => {
      aliveRef.current = false
    }
  }, [])

  /** 拉某一层并并入状态(失败按目录记错误,不影响其余部分)。 */
  const load = useCallback(
    (dir: string) => {
      const revision = (revisionsRef.current.get(dir) ?? 0) + 1
      revisionsRef.current.set(dir, revision)
      setTree((current) => markLoading(current, dir))
      return fetchTree(workspacePath, dir)
        .then((listing) => {
          // 过期响应直接丢弃:同一目录被连续重拉时,先发的可能后到。
          if (!aliveRef.current || revisionsRef.current.get(dir) !== revision) return
          setTree((current) => withListing(current, listing.dir, listing.entries, listing.truncated))
        })
        .catch((cause: unknown) => {
          if (!aliveRef.current || revisionsRef.current.get(dir) !== revision) return
          const message = cause instanceof Error ? cause.message : String(cause)
          setTree((current) => withError(current, dir, message))
        })
    },
    [workspacePath],
  )

  // 工作区或会话变化:整棵树重来(旧根的子项在新根下毫无意义)。
  useEffect(() => {
    setFilter('')
    revisionsRef.current.clear()
    setTree(emptyTree(workspacePath))
    void load('')
  }, [workspacePath, sessionId, load])

  /**
   * 刷新:根目录与**已展开的目录**一起重拉。
   *
   * 只重拉根目录的话,已展开子树里的内容会停在旧状态(用户点了刷新却看不到
   * 变化,还以为刷新没生效)。展开态本身保留,所以这里不需要用户重新展开。
   *
   * 待重拉的目录列表从 `treeRef` 直接算,不依赖 `setTree` 的 updater 同步
   * 执行 —— React 18 里 updater 是在渲染阶段才跑的,靠在 updater 里写外部
   * 变量会拿到空数组(这个坑实测踩到过一次)。
   */
  const refresh = useCallback(() => {
    const reset = resetTree(treeRef.current)
    const targets = ['', ...expandedDirs(reset)]
    setTree(reset)
    for (const dir of targets) void load(dir)
  }, [load])

  const onToggle = useCallback(
    (row: TreeRow) => {
      // 文件行:打开到「文件读取」标签页(目录行的展开/收起不变)。
      if (row.entry.kind !== 'directory') {
        onOpenFile?.(row.entry.path)
        return
      }
      const result = toggleDir(tree, row.entry.path)
      setTree(result.state)
      if (result.needLoad) void load(row.entry.path)
    },
    [tree, load, onOpenFile],
  )

  const rows = useMemo(() => visibleRows(tree), [tree])
  const rootError = dirError(tree, '')
  const rootLoading = tree.dirs['']?.loading === true
  const rootChildren = tree.dirs['']?.children

  /**
   * 筛选:在**已加载的可见行**里按路径子串过滤(不触发新的读盘)。
   *
   * 有意不做全树搜索 —— 那是 @ 提及候选的职责(后端有全树索引)。这里
   * 只是"在眼前这棵树里快速定位",语义更窄但零额外成本,也不会因为一次
   * 输入就把几万个目录读一遍。
   */
  const filtered = useMemo(() => {
    const query = filter.trim().toLowerCase()
    if (!query) return rows
    return rows.filter((row) => row.entry.path.toLowerCase().includes(query))
  }, [rows, filter])

  return (
    <div className="file-tree" data-file-tree-root={tree.root}>
      <div className="file-tree-head">
        <input
          type="search"
          className="file-tree-filter"
          value={filter}
          placeholder={t('sidePaneFilesFilter')}
          aria-label={t('sidePaneFilesFilter')}
          onChange={(event) => setFilter(event.target.value)}
        />
        <button
          type="button"
          className="file-tree-icon-btn"
          title={t('sidePaneFilesRefresh')}
          aria-label={t('sidePaneFilesRefresh')}
          data-file-tree-refresh=""
          onClick={refresh}
        >
          <IconRefresh size={13} />
        </button>
      </div>

      <div className="file-tree-body" role="tree" aria-label={t('sidePaneFiles')}>
        {rootLoading && rootChildren === null && (
          <div className="file-tree-hint">
            <IconSpinner size={12} />
            {t('sidePaneFilesLoading')}
          </div>
        )}
        {rootError && (
          <div className="file-tree-error">
            <p>{rootError}</p>
            <button type="button" className="file-tree-retry" onClick={refresh}>
              {t('sidePaneFilesRetry')}
            </button>
          </div>
        )}
        {!rootLoading && !rootError && rows.length === 0 && (
          <div className="file-tree-hint">{t('sidePaneFilesEmpty')}</div>
        )}
        {!rootError && rows.length > 0 && filtered.length === 0 && (
          <div className="file-tree-hint">{t('sidePaneFilesNoMatch')}</div>
        )}

        {filtered.map((row) => {
          const directory = row.entry.kind === 'directory'
          return (
            <div
              key={row.entry.path}
              className={`file-tree-row${directory ? ' dir' : ''}`}
              data-tree-path={row.entry.path}
              data-tree-kind={row.entry.kind}
              style={{ paddingLeft: `${8 + row.depth * 14}px` }}
              role="treeitem"
              aria-expanded={directory ? row.expanded : undefined}
              draggable
              onDragStart={(event) => {
                // 自定义 MIME:输入框靠它区分"树里拖来的引用"与"外部拖来的文本"。
                event.dataTransfer.setData(
                  REFERENCE_MIME,
                  encodeReference({ path: row.entry.path, kind: row.entry.kind }),
                )
                // 同时给 text/plain:拖到编辑器/终端等外部目标时至少有个可读的路径。
                event.dataTransfer.setData('text/plain', row.entry.path)
                event.dataTransfer.effectAllowed = 'copy'
              }}
              onClick={() => onToggle(row)}
              title={
                directory
                  ? `${row.entry.path} · ${t('sidePaneFilesDragHint')}`
                  : `${row.entry.path} · ${t('sidePaneFilesOpenHint')}`
              }
            >
              <span className="file-tree-caret" aria-hidden="true">
                {directory ? (
                  row.loading ? (
                    <IconSpinner size={11} />
                  ) : (
                    <span className={`file-tree-caret-icon${row.expanded ? ' open' : ''}`}>
                      <IconCaretRight size={11} />
                    </span>
                  )
                ) : null}
              </span>
              <span className="file-tree-icon" aria-hidden="true">
                {directory ? <IconFolder size={13} /> : <IconFile size={13} />}
              </span>
              <span className="file-tree-name">{row.entry.name}</span>
              {row.truncated && (
                <span className="file-tree-truncated" title={t('sidePaneFilesTruncated')}>
                  …
                </span>
              )}
            </div>
          )
        })}

        {/* 展开的目录自己的错误行:只在它被展开时出现 */}
        {rows
          .filter((row) => row.expanded && dirError(tree, row.entry.path))
          .map((row) => (
            <div
              key={`err:${row.entry.path}`}
              className="file-tree-row-error"
              style={{ paddingLeft: `${8 + (row.depth + 1) * 14}px` }}
            >
              <span>{dirError(tree, row.entry.path)}</span>
              <button type="button" className="file-tree-retry" onClick={() => void load(row.entry.path)}>
                {t('sidePaneFilesRetry')}
              </button>
            </div>
          ))}
      </div>
    </div>
  )
}

export default FileTreePanel
