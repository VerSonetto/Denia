/**
 * 收尾轮次的变更卡片。
 *
 * 轮次结束那一刻,对话流需要把"这轮改了什么"讲清楚 —— 这是唯一一处
 * 用户不点任何东西就能拿到的总结。形态取自 Codex 的 turn-diff 卡片:
 * 标题(改了几个/哪个文件) + ±行数 + 可展开的文件列表 + 悬停看 diff。
 *
 * 三处点击区各司其职,不合并:
 * - 头部整行 → 审查面板(工作区全量改动的权威视图);
 * - 文件行   → 文件读取面板(这一份文件的内容);
 * - 文件夹钮 → 平台文件管理器(定位到目录)。
 * 合成一个入口必有一方被牺牲。
 *
 * 悬停预览浮层必须 portal 到 body:会话行带 `content-visibility: auto`,
 * 其 paint containment 会把行内绝对定位的浮层整块裁掉。
 */
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { t } from '../i18n'
import type { TurnProducedFile } from '../fold'
import { useFileOpen } from '../fileOpen'
import { useOpenReview } from '../reviewOpen'
import { openInAppOpen } from '../api'
import { fetchApps } from './OpenInApp'
import { DiffCard, DiffStat } from './DiffCard'
import {
  IconChevron,
  IconChevronDown,
  IconCode,
  IconFile,
  IconFiles,
  IconFolder,
  IconImage,
  IconWrite,
} from './icons'

/** 路径的 basename(兼容 / 与 \):卡片里短名与完整路径分别显示。 */
function pathBasename(path: string): string {
  const idx = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'))
  return idx < 0 ? path : path.slice(idx + 1)
}

/** 收起态最多列出几个文件,其余以「再显示 N 个文件」收尾。
 *  取 3 是权衡:一个列表一行放得下三个文件名还不用滚动,再多就只能靠
 *  展开才看得全,不如让"改了很多"这件事更显眼。 */
const TURN_DIFF_SHOWN_LIMIT = 3

/** 图片扩展名:产物 chip 用图片 glyph(对齐 dsh 的链接图标分类)。 */
const IMAGE_EXTS = new Set(['png', 'jpg', 'jpeg', 'gif', 'webp', 'svg', 'ico', 'bmp', 'avif'])

/** 代码/配置扩展名:产物 chip 用代码 glyph;其余扩展名走纸张文件 glyph。 */
const CODE_EXTS = new Set([
  'ts', 'tsx', 'js', 'jsx', 'mjs', 'cjs', 'py', 'rs', 'go', 'java', 'kt', 'swift',
  'c', 'h', 'cpp', 'hpp', 'cc', 'cs', 'rb', 'php', 'sh', 'bash', 'zsh', 'ps1',
  'json', 'yaml', 'yml', 'toml', 'xml', 'html', 'htm', 'css', 'scss', 'less',
  'sql', 'lua', 'r', 'dart', 'vue', 'svelte', 'proto', 'graphql', 'cmake', 'mk',
])

function fileExtension(path: string): string {
  const name = pathBasename(path)
  const dot = name.lastIndexOf('.')
  return dot <= 0 ? '' : name.slice(dot + 1).toLowerCase()
}

function ProducedFileIcon({ path }: { path: string }) {
  const ext = fileExtension(path)
  if (IMAGE_EXTS.has(ext)) return <IconImage size={12} />
  if (CODE_EXTS.has(ext)) return <IconCode size={12} />
  return <IconFile size={12} />
}

/**
 * 文件所在目录(参数相对路径锚定会话 cwd);绝对路径直接取父目录。
 * 解析不出目录(无 cwd 且相对)返回 null。
 */
function containingDir(path: string, cwd: string | null): string | null {
  const absolute = /^[a-zA-Z]:[\\/]/.test(path) || path.startsWith('/') || path.startsWith('\\\\')
    ? path
    : cwd && cwd.trim() !== ''
      ? `${cwd.replace(/[\\/]+$/, '')}/${path}`
      : null
  if (absolute === null) return null
  const idx = Math.max(absolute.lastIndexOf('/'), absolute.lastIndexOf('\\'))
  return idx > 0 ? absolute.slice(0, idx) : null
}

/** 平台文件管理器的应用 id(open-in-app 目录表里的 ShellOpen 条目)。 */
function platformFileManagerId(): string {
  const ua = navigator.userAgent
  if (ua.includes('Mac')) return 'finder'
  if (ua.includes('Win')) return 'explorer'
  return 'filemanager'
}

/**
 * chip 点击:用平台的文件管理器打开文件所在文件夹。open-in-app 端点目前
 * 只收目录,这是现有端点内能达成的最近动作;用默认应用直接打开文件需要
 * 放开端点的文件路径校验。应用表复用 OpenInApp 的模块级探测缓存。
 */
async function openContainingFolder(path: string, cwd: string | null): Promise<void> {
  const dir = containingDir(path, cwd)
  if (dir === null) return
  const apps = await fetchApps()
  const preferred = platformFileManagerId()
  const app = apps.includes(preferred)
    ? preferred
    : (['explorer', 'finder', 'filemanager'] as const).find((id) => apps.includes(id))
  if (app === undefined) return
  await openInAppOpen(app, dir).catch(() => undefined)
}

/**
 * 单个产物的悬停 diff 预览浮层。
 *
 * 走 **portal + fixed**(与 `.turn-usage-card` 同一手法):会话行带
 * `content-visibility: auto`,其 paint containment 会把行内绝对定位的
 * 浮层整块裁掉,所以浮层必须挂到 body 上、落点由 JS 按视口钳制。
 *
 * 只在指针进入时按需渲染 hunk(懒挂载),不是常驻。
 */
function TurnDiffPreview({
  file,
  anchor,
  onPointerEnter,
  onPointerLeave,
}: {
  file: TurnProducedFile
  /** 定位锚点(文件行):浮层挂载后量它自己的高度,再贴着这行摆。 */
  anchor: HTMLElement | null
  /** 指针进入浮层本体:取消正在进行的关闭计时(见 scheduleClose)。 */
  onPointerEnter: () => void
  /** 指针离开浮层本体:开始关闭计时。 */
  onPointerLeave: () => void
}) {
  const [pos, setPos] = useState<{ left: number; top: number; width: number } | null>(null)
  const ref = useRef<HTMLDivElement | null>(null)

  // 落点必须在浮层挂载**之后**量它自己的高度:挂载前量不到真实高度,
  // 算出来的 top 会把浮层压出屏幕。锚点缺失时(理论上不会,打开时才设)
  // 就停在屏外,等下一次布局再纠正。
  useLayoutEffect(() => {
    const el = ref.current
    if (anchor === null || el === null) return
    const rect = anchor.getBoundingClientRect()
    const width = Math.min(420, window.innerWidth - 16)
    const height = Math.min(300, el.scrollHeight)
    // 优先贴右上;右侧放不下就翻到左侧。上下夹住,不出视口。
    const left = rect.right + 6 + width <= window.innerWidth
      ? rect.right + 6
      : Math.max(8, rect.left - width - 6)
    const top = Math.max(8, Math.min(rect.top, window.innerHeight - height - 8))
    setPos({ left, top, width })
  }, [anchor, file])

  if (file.diffs.length === 0) return null
  return createPortal(
    <div
      ref={ref}
      className="turn-diff-preview"
      role="tooltip"
      style={{
        left: pos?.left ?? -9999,
        top: pos?.top ?? -9999,
        width: pos?.width,
        visibility: pos === null ? 'hidden' : 'visible',
      }}
      onPointerEnter={onPointerEnter}
      onPointerLeave={onPointerLeave}
    >
      <div className="turn-diff-preview-head">
        <span className="turn-diff-preview-path" title={file.path}>
          {file.path}
        </span>
        <DiffStat added={file.added} removed={file.removed} compact />
      </div>
      <div className="turn-diff-preview-body">
        {file.diffs.map((diff, index) => (
          <DiffCard key={index} diff={diff} hidePath />
        ))}
      </div>
    </div>,
    document.body,
  )
}

/**
 * 收尾轮次的变更卡片。
 *
 * 一张卡片回答三个问题:**改了几个文件**(+N/−M 行数)、**是哪些文件**
 * (可展开的文件列表)、**具体改了什么**(行内 diff,单文件直接摊开)。
 *
 * 副标题有两态:默认是 ±行数(事实),悬停换成「查看变更」(可点的意图)——
 * 与 Codex 的 turn-diff 卡片同一套做法。用户悬停时想的是"我要看哪里",
 * 而不是"它加了几行"。
 *
 * 头部整行可点 → 打开审查面板(工作区全量改动的权威视图);文件行可点 →
 * 打开文件读取面板(这一份文件的内容)。两个入口指向不同东西,合成一个
 * 必有一方被牺牲,所以分开放。
 */
export function TurnDiffCard({
  files,
  cwd,
  turn,
  /** 本轮结局(非正常结束时才传):卡片据此挂一道"这轮没跑完"的标记。 */
  interrupted,
}: {
  files: readonly TurnProducedFile[]
  cwd: string | null
  /** 轮次号:面板标题里用来说"第几轮"。 */
  turn?: number
  interrupted?: boolean
}) {
  const openFile = useFileOpen()
  const openReview = useOpenReview()
  // 打开审查面板时把本轮文件一并交出去:面板据此多一个不依赖 git 的
  // 来源。没有这一步,工作区不是仓库时点开就是空面板。
  const showChanges = useCallback(
    (focusPath?: string) => openReview({ files, turn, focusPath }),
    [openReview, files, turn],
  )
  const [expanded, setExpanded] = useState(false)
  const [preview, setPreview] = useState<{ file: TurnProducedFile; anchor: HTMLElement } | null>(null)
  // 浮层不是触发器的 DOM 子节点(它 portal 在 body 上),指针从行移到
  // 浮层的路上必然先离开行 —— 所以"离开"不能立刻关,要留一段让指针
  // 走过去的时间。220ms 够跨过这段空隙,又不至于让"想关掉"变得拖沓。
  const closeTimer = useRef<number | null>(null)
  const CLOSE_DELAY = 220

  const cancelClose = useCallback(() => {
    if (closeTimer.current !== null) {
      window.clearTimeout(closeTimer.current)
      closeTimer.current = null
    }
  }, [])
  const scheduleClose = useCallback(() => {
    cancelClose()
    closeTimer.current = window.setTimeout(() => {
      closeTimer.current = null
      setPreview(null)
    }, CLOSE_DELAY)
  }, [cancelClose])

  // 卡片卸载时把定时器清掉:否则会对着已卸载的组件 setState。
  useEffect(() => cancelClose, [cancelClose])

  const openPreview = useCallback((file: TurnProducedFile, anchor: HTMLElement) => {
    cancelClose()
    setPreview({ file, anchor })
  }, [cancelClose])

  if (files.length === 0) return null

  const single = files.length === 1 ? files[0] : null
  const added = files.reduce((sum, file) => sum + file.added, 0)
  const removed = files.reduce((sum, file) => sum + file.removed, 0)
  const shown = expanded ? files : files.slice(0, TURN_DIFF_SHOWN_LIMIT)
  const hidden = files.length - shown.length
  const title = single !== null
    ? single.kind === 'write'
      ? t('turnDiffCreated', { name: pathBasename(single.path) })
      : t('turnDiffEditedOne', { name: pathBasename(single.path) })
    : t('turnDiffEdited', { count: files.length })

  return (
    <div className="turn-diff-card">
      <div className="turn-diff-head">
        {/* 整行可点的透明覆盖层:语义是"这轮改动的全景",落点比一个
            小按钮大得多,但不该让副标题/标题抢到点击。 */}
        <button
          type="button"
          className="turn-diff-head-hit"
          aria-label={t('turnDiffViewAll')}
          onClick={() => showChanges()}
        />
        <span className="turn-diff-icon" aria-hidden>
          <IconWrite size={13} />
        </span>
        <span className="turn-diff-titles">
          <span className="turn-diff-title" title={single?.path}>
            {title}
            {/* 中断轮次:这半句挂在标题末尾而不是单独一行 —— 结局与变更
                本是一件事的两面(见 Codex 的 noDiff.reverted 文案),但
                各占一行会被读成两条并列信息。 */}
            {interrupted === true && (
              <span className="turn-diff-partial" title={t('turnDiffPartialHint')}>
                {t('turnDiffPartial')}
              </span>
            )}
          </span>
          <span className="turn-diff-sub">
            <span className="turn-diff-sub-default">
              <DiffStat added={added} removed={removed} />
            </span>
            <span className="turn-diff-sub-hover">
              {t('turnDiffViewChanges')}
              <IconChevron size={11} />
            </span>
          </span>
        </span>
        <button
          type="button"
          className="turn-diff-view-btn"
          onClick={(event) => {
            event.stopPropagation()
            showChanges()
          }}
        >
          {t('turnDiffViewChanges')}
        </button>
      </div>

      {files.length > 1 && (
        <div className="turn-diff-list">
          {shown.map((file) => {
            const name = pathBasename(file.path)
            const dir = file.path.slice(0, file.path.length - name.length)
            return (
              <div
                key={file.path}
                className="turn-diff-file"
                onPointerEnter={(event) => openPreview(file, event.currentTarget)}
                onPointerLeave={scheduleClose}
              >
                <button
                  type="button"
                  className="turn-diff-file-main"
                  title={`${file.path} · ${t('openInFileReader')}`}
                  aria-label={t('turnDiffOpenFile', { name })}
                  onClick={() => openFile(file.path)}
                >
                  <ProducedFileIcon path={file.path} />
                  <span className="turn-diff-file-path" aria-hidden>
                    {dir.length > 0 && <span className="turn-diff-file-dir">{dir}</span>}
                    <span className="turn-diff-file-name">{name}</span>
                  </span>
                  <DiffStat added={file.added} removed={file.removed} compact />
                </button>
                {/* 两个次级入口都与主按钮不同意图,所以各占一个点击区,
                    并在 hover 该行时才显形,不去和主按钮争视线:
                    - 在审查面板看这个文件的改动(与本轮 diff 同一份数据);
                    - 在文件管理器里定位。 */}
                <button
                  type="button"
                  className="turn-diff-file-folder"
                  title={t('turnDiffViewInReview', { name })}
                  aria-label={t('turnDiffViewInReview', { name })}
                  onClick={() => showChanges(file.path)}
                >
                  <IconFiles size={12} />
                </button>
                <button
                  type="button"
                  className="turn-diff-file-folder"
                  title={t('producedFilesOpenFolder', { name })}
                  aria-label={t('producedFilesOpenFolder', { name })}
                  onClick={() => { void openContainingFolder(file.path, cwd) }}
                >
                  <IconFolder size={12} />
                </button>
              </div>
            )
          })}
          {hidden > 0 || expanded ? (
            <button
              type="button"
              className={`turn-diff-toggle${expanded ? ' open' : ''}`}
              aria-expanded={expanded}
              onClick={() => setExpanded(!expanded)}
            >
              {expanded
                ? t('turnDiffCollapse')
                : t('turnDiffShowMore', { count: hidden })}
              <IconChevronDown size={11} />
            </button>
          ) : null}
        </div>
      )}

      {/* 单文件:直接把本轮对它的 diff 摊在卡片里。多文件摊不开(每个都
          几十行),只留文件行 + 悬停预览。 */}
      {single !== null && single.diffs.length > 0 && (
        <div className="turn-diff-inline">
          {single.diffs.map((diff, index) => (
            <DiffCard key={index} diff={diff} hidePath />
          ))}
        </div>
      )}

      {preview !== null && (
        <TurnDiffPreview
          file={preview.file}
          anchor={preview.anchor}
          onPointerEnter={cancelClose}
          onPointerLeave={scheduleClose}
        />
      )}
    </div>
  )
}
