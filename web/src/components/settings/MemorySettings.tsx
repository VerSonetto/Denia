import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { IconCheck } from '../icons'
import styles from './MemorySettings.module.css'

type PreviewError = {
  kind: 'not-found' | 'too-large' | 'bad-path' | 'other'
  message: string
}

/** 相对时间:分钟内"刚刚",一小时内分钟,当天内小时,其余天;再旧回退日期。 */
function formatRelative(ms: number): string {
  if (!ms) return '—'
  const diff = Date.now() - ms
  const minute = 60_000
  const hour = 60 * minute
  const day = 24 * hour
  if (diff < minute) return '刚刚'
  if (diff < hour) return `${Math.floor(diff / minute)} 分钟前`
  if (diff < day) return `${Math.floor(diff / hour)} 小时前`
  if (diff < 30 * day) return `${Math.floor(diff / day)} 天前`
  const date = new Date(ms)
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(
    date.getDate(),
  ).padStart(2, '0')}`
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

export function MemorySettings() {
  // —— 开关(runtime.memoryEnabled)——
  const [enabled, setEnabled] = useState<boolean | null>(null)
  const [runtimeValue, setRuntimeValue] = useState<Record<string, unknown> | null>(null)
  const [revision, setRevision] = useState(0)
  const [toggleBusy, setToggleBusy] = useState(false)
  const [toggleError, setToggleError] = useState('')
  const [saved, setSaved] = useState(false)

  // —— viewer ——
  const [projects, setProjects] = useState<api.MemoryProject[] | null>(null)
  const [projectsError, setProjectsError] = useState('')
  const [selectedId, setSelectedId] = useState('')
  const [search, setSearch] = useState('')
  const [selectedFile, setSelectedFile] = useState<string | null>(null)
  const [preview, setPreview] = useState<api.MemoryFileView | null>(null)
  const [previewLoading, setPreviewLoading] = useState(false)
  const [previewError, setPreviewError] = useState<PreviewError | null>(null)
  const [changedDuringRead, setChangedDuringRead] = useState(false)
  const [workspaceOpen, setWorkspaceOpen] = useState(false)
  // 请求序号:慢请求返回时丢弃过期结果。
  const requestSeq = useRef(0)

  useEffect(() => {
    let disposed = false
    api
      .getSettings()
      .then((d) => {
        if (disposed) return
        const ns = d.namespaces.find((n) => n.ns === 'runtime')
        if (ns) {
          setRuntimeValue(ns.value)
          setRevision(ns.revision)
          setEnabled(ns.value?.memoryEnabled !== false)
        }
      })
      .catch((e) => {
        if (!disposed) setToggleError(String(e))
      })
    return () => {
      disposed = true
    }
  }, [])

  const loadProjects = useCallback(async () => {
    setProjectsError('')
    try {
      const data = await api.getMemoryProjects()
      setProjects(data.projects)
      setSelectedId((current) => {
        if (current && data.projects.some((project) => project.id === current)) return current
        return data.projects[0]?.id ?? ''
      })
    } catch (e) {
      setProjectsError(e instanceof Error ? e.message : String(e))
    }
  }, [])

  useEffect(() => {
    void loadProjects()
  }, [loadProjects])

  const toggleEnabled = async (next: boolean) => {
    if (!runtimeValue) return
    setToggleBusy(true)
    setToggleError('')
    setSaved(false)
    const previous = enabled
    setEnabled(next)
    try {
      const view = await api.replaceNamespace(
        'runtime',
        { ...runtimeValue, memoryEnabled: next },
        revision,
      )
      setRevision((view as { revision: number }).revision)
      setRuntimeValue((current) => (current ? { ...current, memoryEnabled: next } : current))
      setSaved(true)
    } catch (e) {
      setEnabled(previous)
      setToggleError(e instanceof Error ? e.message : String(e))
    } finally {
      setToggleBusy(false)
    }
  }

  const selected = useMemo(
    () => projects?.find((project) => project.id === selectedId) ?? null,
    [projects, selectedId],
  )

  const visibleFiles = useMemo(() => {
    const files = selected?.files ?? []
    const needle = search.trim().toLowerCase()
    if (!needle) return files
    return files.filter((file) => file.name.toLowerCase().includes(needle))
  }, [selected, search])

  const openFile = useCallback(
    async (file: api.MemoryFileInfo) => {
      const seq = ++requestSeq.current
      setSelectedFile(file.name)
      setPreview(null)
      setPreviewError(null)
      setChangedDuringRead(false)
      setPreviewLoading(true)
      try {
        const view = await api.getMemoryFile(selectedId, file.name)
        if (requestSeq.current !== seq) return
        setPreview(view)
        setChangedDuringRead(view.modifiedMs !== file.modifiedMs)
      } catch (e) {
        if (requestSeq.current !== seq) return
        const err = e as Error & { code?: string; status?: number }
        const code = err.code ?? ''
        const kind: PreviewError['kind'] = code.includes('too-large')
          ? 'too-large'
          : code.includes('not-found')
            ? 'not-found'
            : code.includes('bad-path')
              ? 'bad-path'
              : 'other'
        setPreviewError({ kind, message: err.message })
      } finally {
        if (requestSeq.current === seq) setPreviewLoading(false)
      }
    },
    [selectedId],
  )

  /** 切换工作区:已选文件与预览一并复位(预览内容属于旧工作区)。 */
  const selectWorkspace = useCallback((id: string) => {
    setSelectedId(id)
    setSelectedFile(null)
    setPreview(null)
    setPreviewError(null)
    setChangedDuringRead(false)
  }, [])

  return (
    <section className="setm-section">
      {/* —— 开关 —— */}
      <div className={styles.toggleRow}>
        <label className={styles.toggle}>
          <input
            type="checkbox"
            checked={enabled ?? false}
            disabled={toggleBusy || enabled === null}
            onChange={(event) => void toggleEnabled(event.target.checked)}
          />
          <span className={styles.toggleLabel}>{t('memoryToggleLabel')}</span>
        </label>
        {saved && <span className={styles.saved}>{t('memoryToggleSaved')}</span>}
      </div>
      <p className={styles.hint}>{t('memoryToggleHint')}</p>
      {toggleError && (
        <p className={styles.error} role="alert">
          {toggleError}
        </p>
      )}

      {/* —— 只读 viewer —— */}
      <div className={styles.viewer}>
        <div className={styles.viewerHead}>
          <span className={styles.viewerTitle}>{t('memoryViewerTitle')}</span>
          {/* 原生 select 的弹层管不到样式,自定义弹出列表(同 ui-dropdown 主题)。 */}
          <div className={styles.dropdown}>
            <button
              type="button"
              className={styles.dropdownTrigger}
              disabled={!projects || projects.length === 0}
              aria-expanded={workspaceOpen}
              aria-haspopup="listbox"
              onClick={() => setWorkspaceOpen((state) => !state)}
            >
              <span className={styles.dropdownValue}>
                {selected ? selected.title || selected.path : '—'}
              </span>
            </button>
            {workspaceOpen && (
              <>
                <div className="menu-backdrop" onClick={() => setWorkspaceOpen(false)} />
                <div
                  className={`ui-dropdown-menu ${styles.menu}`}
                  role="listbox"
                  aria-label={t('memoryWorkspaceLabel')}
                >
                  {(projects ?? []).map((project) => {
                    const selected = project.id === selectedId
                    return (
                      <button
                        key={project.id}
                        type="button"
                        role="option"
                        aria-selected={selected}
                        className={`ui-dropdown-item ${styles.menuItem}`}
                        onClick={() => {
                          selectWorkspace(project.id)
                          setWorkspaceOpen(false)
                        }}
                      >
                        <span className={styles.menuItemText}>
                          <span className={`name${selected ? ` ${styles.nameSelected}` : ''}`}>
                            {project.title || project.path}
                          </span>
                          {project.title && project.title !== project.path && (
                            <span className={`hint ${styles.menuHint}`}>{project.path}</span>
                          )}
                        </span>
                        {selected && (
                          <span className={styles.checkWrap}>
                            <IconCheck size={13} />
                          </span>
                        )}
                      </button>
                    )
                  })}
                </div>
              </>
            )}
          </div>
          <input
            className={styles.search}
            type="search"
            value={search}
            placeholder={t('memorySearchPlaceholder')}
            onChange={(event) => setSearch(event.target.value)}
          />
        </div>

        {projectsError ? (
          <p className={styles.error} role="alert">
            {projectsError}
          </p>
        ) : projects === null ? (
          <p className={styles.empty}>{t('loading')}</p>
        ) : projects.length === 0 ? (
          <p className={styles.empty}>{t('memoryNoProjects')}</p>
        ) : (
          <div className={styles.columns}>
            <ul className={styles.fileList}>
              {visibleFiles.length === 0 ? (
                <li className={styles.empty}>{t('memoryNoFiles')}</li>
              ) : (
                visibleFiles.map((file) => (
                  <li key={file.name}>
                    <button
                      type="button"
                      className={`${styles.fileItem}${selectedFile === file.name ? ' active' : ''}`}
                      onClick={() => void openFile(file)}
                    >
                      <span className={styles.fileName}>{file.name}</span>
                      <span className={styles.fileMeta}>
                        {formatSize(file.size)} · {formatRelative(file.modifiedMs)}
                      </span>
                    </button>
                  </li>
                ))
              )}
            </ul>
            <div className={styles.preview}>
              {previewLoading ? (
                <p className={styles.empty}>{t('memoryPreviewLoading')}</p>
              ) : previewError ? (
                <p className={styles.previewError} role="alert">
                  {previewError.kind === 'not-found'
                    ? t('memoryFileNotFound')
                    : previewError.kind === 'too-large'
                      ? t('memoryFileTooLarge')
                      : previewError.kind === 'bad-path'
                        ? t('memoryBadPath')
                        : `${t('memoryReadFailed')}:${previewError.message}`}
                </p>
              ) : preview ? (
                <>
                  {changedDuringRead && (
                    <p className={styles.changedHint} role="status">
                      {t('memoryFileChanged')}
                    </p>
                  )}
                  <pre className={styles.previewBody}>{preview.content}</pre>
                </>
              ) : (
                <p className={styles.empty}>{t('memoryPreviewEmpty')}</p>
              )}
            </div>
          </div>
        )}
      </div>
    </section>
  )
}
