/**
 * 文件读取标签页:只读查看工作区内的文件内容。
 *
 * # 为什么没有手动入口
 *
 * 这个标签页**不出现在 `+` 菜单里**(见 `panelRegistry.ts` 的 `available:
 * () => false`),也不能手输路径。它只能由四类点击触发打开:
 * 对话流里的 Markdown 文件引用、edit/write 工具行的路径、轮次产物列表、
 * 侧栏工作区文件树里的文件项。这样它永远只承载"用户刚刚真的点了的东西",
 * 不会变成第二个需要维护路径输入的地址栏。
 *
 * # 条目模型
 *
 * 已打开的文件存在标签自己的 `files` 数组里(见 `sidePane.ts` 的
 * `OpenedFile`),而不是单独一份 store —— 标签关掉条目就跟着消失,不需要
 * 额外清理路径。同一文件重复点击复用已有条目(不重复添加),只切换激活项。
 *
 * # 三态
 *
 * 加载中 / 加载失败 / 已加载。只保留当前文件正文,切换时取消旧请求并
 * 回收正文;文件标签保留,再次激活时重新读取,失败只影响当前文件。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { t } from '../i18n'
import * as api from '../api'
import type { OpenedFile } from '../sidePane'
import { CodeBlock } from '../markdown/CodeBlock'
import { IconClose, IconFile, IconRefresh, IconSpinner } from './icons'
import './FileReaderPanel.css'

/** 当前文件的加载状态。 */
type FileState =
  | { kind: 'loading' }
  | { kind: 'ready'; view: api.WorkspaceFileView }
  | { kind: 'error'; message: string; code: string }

export interface FileReaderPanelProps {
  /** 工作区绝对路径(文件读取的根)。 */
  workspacePath: string
  /** 已打开的文件条目(顺序即条目标签顺序)。 */
  files: readonly OpenedFile[]
  /** 当前激活的文件路径。 */
  activeFile: string | undefined
  /** 切换激活条目。 */
  onActivate(path: string): void
  /** 关闭一个条目(最后一个会连带关掉标签页)。 */
  onCloseFile(path: string): void
}

export function FileReaderPanel({
  workspacePath,
  files,
  activeFile,
  onActivate,
  onCloseFile,
}: FileReaderPanelProps) {
  /**
   * 只驻留当前文件的正文;后台标签仅保留路径与名称。
   */
  const [states, setStates] = useState<Record<string, FileState>>({})
  /**
   * 单调递增的请求代号:快速切换和重试时,慢响应不能覆盖新状态。
   */
  const revisionRef = useRef(0)
  const requestRef = useRef<AbortController | null>(null)
  const tabsRef = useRef<HTMLDivElement | null>(null)
  const aliveRef = useRef(true)

  useEffect(() => {
    aliveRef.current = true
    return () => {
      aliveRef.current = false
      requestRef.current?.abort()
    }
  }, [])

  const load = useCallback(
    (path: string) => {
      const revision = ++revisionRef.current
      requestRef.current?.abort()
      const controller = new AbortController()
      requestRef.current = controller
      setStates({ [path]: { kind: 'loading' } })
      api
        .fetchWorkspaceFile(workspacePath, path, controller.signal)
        .then((view) => {
          if (!aliveRef.current || controller.signal.aborted || revisionRef.current !== revision) return
          setStates({ [path]: { kind: 'ready', view } })
        })
        .catch((cause: unknown) => {
          if (!aliveRef.current || controller.signal.aborted || revisionRef.current !== revision) return
          const message = cause instanceof Error ? cause.message : String(cause)
          const code = cause instanceof api.ApiError ? cause.code : ''
          setStates({ [path]: { kind: 'error', message, code } })
        })
    },
    [workspacePath],
  )

  // 工作区换了(切会话):整批状态作废 —— 旧根的路径在新根下毫无意义。
  useEffect(() => {
    requestRef.current?.abort()
    setStates({})
  }, [workspacePath])

  // 按需加载:只为"还没有状态"的条目发请求。已加载/加载中/失败的都不重发
  // (失败要重试由用户点重试按钮,不自动重试)。
  const active = activeFile ?? files[files.length - 1]?.path ?? ''
  useEffect(() => {
    requestRef.current?.abort()
    setStates((current) => {
      const next: Record<string, FileState> = {}
      for (const file of files) {
        const state = current[file.path]
        if (file.path === active && state) next[file.path] = state
      }
      return Object.keys(current).every((path) => next[path] === current[path]) ? current : next
    })
  }, [active])
  useEffect(() => {
    if (active && (states[active] === undefined ||
      (states[active].kind === 'loading' && requestRef.current?.signal.aborted))) load(active)
  }, [active, states, load])
  const state = states[active]
  const activeEntry = useMemo(
    () => files.find((file) => file.path === active) ?? null,
    [files, active],
  )

  useEffect(() => {
    const tabs = tabsRef.current
    if (!tabs || !activeFile) return
    const entry = tabs.querySelector<HTMLElement>(
      `[data-file-entry="${CSS.escape(activeFile)}"]`,
    )
    entry?.scrollIntoView({ block: 'nearest', inline: 'nearest' })
  }, [activeFile, files])

  return (
    <div className="file-reader" data-file-reader-root={workspacePath}>
      {/* 内部条目标签:文件多开时靠它切换。最后一个条目关掉后标签页自己
          消失(见 sidePane.closeFile),所以这里不需要空态分支。 */}
      {files.length > 1 && (
        <div
          ref={tabsRef}
          className="file-reader-tabs"
          role="tablist"
          aria-label={t('sidePaneFileTabs')}
          onWheel={(event) => {
            // 文件标签只横向溢出；把普通鼠标滚轮转成横向滚动，
            // 触控板已有的横向滚动则保持原方向。
            const delta = Math.abs(event.deltaX) >= Math.abs(event.deltaY)
              ? event.deltaX
              : event.deltaY
            if (delta === 0) return
            const tabs = event.currentTarget
            const maxScroll = tabs.scrollWidth - tabs.clientWidth
            if (maxScroll <= 0) return
            const next = Math.max(0, Math.min(maxScroll, tabs.scrollLeft + delta))
            if (next === tabs.scrollLeft) return
            event.preventDefault()
            tabs.scrollLeft = next
          }}
        >
          {files.map((file) => {
            const isActive = file.path === active
            return (
              <div
                key={file.path}
                className={`file-reader-tab${isActive ? ' active' : ''}`}
                data-file-entry={file.path}
              >
                <button
                  type="button"
                  role="tab"
                  aria-selected={isActive}
                  className="file-reader-tab-main"
                  title={file.path}
                  onClick={() => onActivate(file.path)}
                >
                  <IconFile size={12} />
                  <span className="file-reader-tab-name">{file.name}</span>
                </button>
                <button
                  type="button"
                  className="file-reader-tab-close"
                  title={t('sidePaneFileCloseEntry')}
                  aria-label={t('sidePaneFileCloseEntry')}
                  onClick={() => onCloseFile(file.path)}
                >
                  <IconClose size={10} />
                </button>
              </div>
            )
          })}
        </div>
      )}

      <div className="file-reader-head">
        <span className="file-reader-path" title={active}>
          {active}
        </span>
        {state?.kind === 'ready' && (
          <span className="file-reader-meta">
            {t('sidePaneFileLines', { count: state.view.lines })}
            {' · '}
            {formatBytes(state.view.size)}
          </span>
        )}
        <button
          type="button"
          className="file-reader-icon-btn"
          title={t('sidePaneFileReload')}
          aria-label={t('sidePaneFileReload')}
          data-file-reader-reload=""
          onClick={() => active && load(active)}
        >
          <IconRefresh size={13} />
        </button>
        {files.length === 1 && activeEntry && (
          <button
            type="button"
            className="file-reader-icon-btn"
            title={t('sidePaneFileCloseEntry')}
            aria-label={t('sidePaneFileCloseEntry')}
            onClick={() => onCloseFile(activeEntry.path)}
          >
            <IconClose size={13} />
          </button>
        )}
      </div>

      <div className="file-reader-body">
        {state === undefined || state.kind === 'loading' ? (
          <div className="file-reader-hint">
            <IconSpinner size={12} />
            {t('sidePaneFileLoading')}
          </div>
        ) : state.kind === 'error' ? (
          <div className="file-reader-error" data-file-reader-error={state.code || 'unknown'}>
            <p>{state.message}</p>
            <button type="button" className="file-reader-retry" onClick={() => load(active)}>
              {t('sidePaneFileRetry')}
            </button>
          </div>
        ) : (
          <div className="file-reader-code">
            {/* 复用对话流那套 shiki 高亮:同一份主题变量、同一套按需加载
                语法(见 markdown/highlight.ts),不引第二个高亮器。
                语言推不出时传空串 → 按纯文本渲染,仍然是等宽。 */}
            <CodeBlock
              code={state.view.content}
              lang={state.view.language ?? ''}
              copyLabel={t('copy')}
              copiedLabel={t('copied')}
            />
          </div>
        )}
      </div>
    </div>
  )
}

/** 人类可读的字节数(与文件树/产物行同一套口径)。 */
function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

export default FileReaderPanel
