import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type ReactNode, type RefObject } from 'react'
import * as api from '../api'
import type { MentionCandidate } from '../api'
import { sameWorkspacePath } from '../features/conversation/composerContext'
import { t } from '../i18n'
import type { SessionSummary } from '../types'
import { ChatCircleText, ClipboardText, FileText, FolderOpen, MagnifyingGlass, Paperclip, Plus, Target, Terminal, X } from '@phosphor-icons/react'
import { PanePopover } from './PanePopover'
import './ComposerContextMenu.css'

export interface ComposerContextMenuProps {
  disabled?: boolean
  filesAvailable?: boolean
  panelAnchorRef?: RefObject<HTMLElement | null>
  workspacePath: string | null
  activeId: string | null
  sessions: SessionSummary[]
  commands: Array<{ name: string; label: string; description: string }>
  onOpen(): void
  onUpload(): void
  onFile(candidate: MentionCandidate): void
  onSession(session: SessionSummary, signal: AbortSignal): Promise<void>
  onCommand(name: string): void
}

interface MenuItem {
  key: string
  kind: 'upload' | 'command' | 'file' | 'session' | 'retry'
  label: string
  detail?: string
  icon: ReactNode
  select(): void
  path?: string
  sessionId?: string
  command?: string
}

const errorMessage = (error: unknown) => error instanceof Error ? error.message : String(error)

/** The page owns insertion and editor focus; this menu only selects context. */
export function ComposerContextMenu(props: ComposerContextMenuProps) {
  const { disabled = false, filesAvailable = true, workspacePath, activeId, sessions, commands } = props
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState('')
  const [files, setFiles] = useState<MentionCandidate[]>([])
  const [loading, setLoading] = useState(false)
  const [fileError, setFileError] = useState('')
  const [retry, setRetry] = useState(0)
  const [pending, setPending] = useState<string | null>(null)
  const [sessionError, setSessionError] = useState<{ session: SessionSummary; message: string } | null>(null)
  const [activeKey, setActiveKey] = useState('upload')
  const navigationRef = useRef({ key: 'upload', index: 0 })
  const anchorRef = useRef<HTMLButtonElement>(null)
  const searchRef = useRef<HTMLInputElement>(null)
  const menuRef = useRef<HTMLDivElement>(null)
  const debounceRef = useRef<number | null>(null)
  const searchAbortRef = useRef<AbortController | null>(null)
  const sessionAbortRef = useRef<AbortController | null>(null)
  const openRef = useRef(false)
  const composingRef = useRef(false)
  const latestRef = useRef(props)
  latestRef.current = props
  const id = useId()
  const menuId = `${id}-menu`
  const hintId = `${id}-hint`
  const searchQuery = query.trim()
  const needle = searchQuery.toLocaleLowerCase()
  const visible = open && !disabled

  const cancelSearch = useCallback(() => {
    if (debounceRef.current !== null) window.clearTimeout(debounceRef.current)
    debounceRef.current = null
    searchAbortRef.current?.abort()
    searchAbortRef.current = null
  }, [])

  const close = useCallback((restoreFocus = false) => {
    openRef.current = false
    cancelSearch()
    sessionAbortRef.current?.abort()
    sessionAbortRef.current = null
    composingRef.current = false
    setOpen(false)
    setFiles([])
    setLoading(false)
    setPending(null)
    setSessionError(null)
    if (restoreFocus) anchorRef.current?.focus({ preventScroll: true })
  }, [cancelSearch])
  // Outside clicks must not steal focus from their destination.
  const dismiss = useCallback(() => close(), [close])

  useLayoutEffect(() => {
    close()
  }, [workspacePath, disabled, activeId, close])

  useLayoutEffect(() => {
    if (visible) searchRef.current?.focus({ preventScroll: true })
  }, [visible])

  useEffect(() => {
    cancelSearch()
    setFiles([])
    setFileError('')
    setLoading(false)
    if (!visible || !workspacePath || !filesAvailable) return
    const controller = new AbortController()
    searchAbortRef.current = controller
    setLoading(true)
    debounceRef.current = window.setTimeout(() => {
      debounceRef.current = null
      const current = () => !controller.signal.aborted && searchAbortRef.current === controller
        && openRef.current && !latestRef.current.disabled && latestRef.current.filesAvailable !== false
        && latestRef.current.workspacePath === workspacePath
      void api.searchMentions(workspacePath, searchQuery, controller.signal).then(({ items }) => {
        if (!current()) return
        setFiles(searchQuery ? items : items.slice(0, 10))
        setLoading(false)
      }).catch((error: unknown) => {
        if (!current()) return
        setFileError(errorMessage(error))
        setLoading(false)
      })
    }, 150)
    return cancelSearch
  }, [visible, workspacePath, filesAvailable, query, searchQuery, retry, cancelSearch])

  // Capture before PanePopover's document Escape handler, including focus that has tabbed out.
  // Composition is never treated as a menu shortcut.
  useEffect(() => {
    if (!visible) return
    const onEscape = (event: globalThis.KeyboardEvent) => {
      if (event.key !== 'Escape') return
      event.stopImmediatePropagation()
      if (composingRef.current || event.isComposing || event.keyCode === 229) return
      event.preventDefault()
      close(true)
    }
    document.addEventListener('keydown', onEscape, true)
    return () => document.removeEventListener('keydown', onEscape, true)
  }, [visible, close])

  useEffect(() => () => {
    openRef.current = false
    cancelSearch()
    sessionAbortRef.current?.abort()
  }, [cancelSearch])

  const canSelect = () => openRef.current && !latestRef.current.disabled && !sessionAbortRef.current
  const pickSession = async (session: SessionSummary) => {
    if (!canSelect()) return
    const controller = new AbortController()
    sessionAbortRef.current = controller
    setPending(session.id)
    setSessionError(null)
    try {
      await props.onSession(session, controller.signal)
      if (controller.signal.aborted || sessionAbortRef.current !== controller || !openRef.current
        || latestRef.current.disabled || latestRef.current.workspacePath !== workspacePath) return
      sessionAbortRef.current = null
      close() // onSession has already restored the editor's focus.
    } catch (error: unknown) {
      if (controller.signal.aborted || sessionAbortRef.current !== controller || !openRef.current
        || latestRef.current.disabled || latestRef.current.workspacePath !== workspacePath) return
      sessionAbortRef.current = null
      setPending(null)
      setSessionError({ session, message: errorMessage(error) })
    }
  }

  const additions: MenuItem[] = [{
    key: 'upload', kind: 'upload', label: t('uploadFile'), detail: t('contextUploadHint'), icon: <Paperclip size={20} weight="duotone" />,
    select: () => {
      if (!canSelect()) return
      // No await/timer before the file picker: retain the original user gesture.
      props.onUpload()
      close()
    },
  }, ...commands.filter(command => !needle || [command.name, command.label, command.description]
    .some(text => text.toLocaleLowerCase().includes(needle))).map(command => ({
    key: `command:${command.name}`, kind: 'command' as const, command: command.name,
    label: command.label, detail: command.description, icon: command.name === 'goal' ? <Target size={20} weight="duotone" />
      : command.name === 'plan' ? <ClipboardText size={20} weight="duotone" /> : <Terminal size={20} weight="duotone" />,
    select: () => { if (canSelect()) { props.onCommand(command.name); close() } },
  }))]
  const fileItems: MenuItem[] = (workspacePath && filesAvailable && !loading && !fileError ? files : []).map(candidate => ({
    key: `file:${candidate.kind}:${candidate.path}`, kind: 'file', path: candidate.path,
    label: candidate.path + (candidate.kind === 'directory' && !candidate.path.endsWith('/') ? '/' : ''),
    icon: candidate.kind === 'directory' ? <FolderOpen size={20} weight="duotone" /> : <FileText size={20} weight="duotone" />,
    select: () => {
      if (canSelect() && latestRef.current.filesAvailable !== false && latestRef.current.workspacePath === workspacePath) {
        props.onFile(candidate)
        close()
      }
    },
  }))
  const sessionItems: MenuItem[] = sessions.filter(session => session.id !== activeId
    && sameWorkspacePath(session.cwd, workspacePath)
    && (!needle || [session.title, session.id, session.excerpt].some(text => text?.toLocaleLowerCase().includes(needle))))
    .slice(0, 10).map(session => ({
      key: `session:${session.id}`, kind: 'session', sessionId: session.id,
      label: session.title?.trim() || session.excerpt?.trim() || session.id,
      detail: pending === session.id ? t('contextLoading') : `${session.id} · ${t('contextSessionHint')}`,
      icon: <ChatCircleText size={20} weight="duotone" />, select: () => { void pickSession(session) },
    }))
  const fileRetry: MenuItem | null = fileError ? {
    key: 'file-retry', kind: 'retry', label: t('contextRetry'), icon: <FolderOpen size={20} weight="duotone" />,
    select: () => { if (canSelect()) { cancelSearch(); setRetry(value => value + 1) } },
  } : null
  const sessionRetry: MenuItem | null = sessionError ? {
    key: 'session-retry', kind: 'retry', label: t('contextRetry'), icon: <ChatCircleText size={20} weight="duotone" />,
    select: () => { void pickSession(sessionError.session) },
  } : null
  const items = [...additions, ...(fileRetry ? [fileRetry] : fileItems), ...(sessionRetry ? [sessionRetry] : []), ...sessionItems]
  const stableIndex = items.findIndex(item => item.key === activeKey)
  const selectedIndex = stableIndex >= 0 ? stableIndex
    : Math.min(navigationRef.current.key === activeKey ? navigationRef.current.index : 0, items.length - 1)
  const selectedKey = items[selectedIndex]?.key
  const itemId = (key: string) => `${id}-item-${encodeURIComponent(key)}`

  useLayoutEffect(() => {
    if (!selectedKey) return
    navigationRef.current = { key: selectedKey, index: selectedIndex }
    if (activeKey !== selectedKey) setActiveKey(selectedKey)
  }, [activeKey, selectedKey, selectedIndex])

  useEffect(() => {
    if (visible) menuRef.current?.querySelector<HTMLElement>('[data-kb="true"]')?.scrollIntoView?.({ block: 'nearest' })
  }, [visible, selectedKey, files, query, pending, sessionError, fileError])

  const onKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    // Portal events still bubble to the composer. In particular, search Enter must never send a message.
    event.stopPropagation()
    if (composingRef.current || event.nativeEvent.isComposing || event.nativeEvent.keyCode === 229) return
    if (event.key === 'Escape') { event.preventDefault(); close(true); return }
    const target = event.target as HTMLElement
    if (target !== searchRef.current && target !== menuRef.current && !target.closest('[data-context-item]')) return
    if (!['ArrowUp', 'ArrowDown', 'Home', 'End', 'Enter'].includes(event.key)) return
    event.preventDefault()
    if (pending || !items.length) return
    const current = Math.max(0, items.findIndex(item => item.key === selectedKey))
    if (event.key === 'Enter') { items[current].select(); return }
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? items.length - 1
      : (current + (event.key === 'ArrowDown' ? 1 : -1) + items.length) % items.length
    setActiveKey(items[next].key)
    if (target !== searchRef.current) document.getElementById(itemId(items[next].key))?.focus({ preventScroll: true })
  }

  const renderItem = (item: MenuItem) => <button
    key={item.key} id={itemId(item.key)} type="button" role="menuitem"
    className="composer-context-item" disabled={pending !== null}
    data-context-item={item.kind} data-context-path={item.path} data-context-session={item.sessionId}
    data-context-command={item.command} data-kb={selectedKey === item.key ? 'true' : 'false'}
    onFocus={() => setActiveKey(item.key)} onMouseEnter={() => { if (!pending) setActiveKey(item.key) }}
    onClick={item.select}
  >
    <span className="composer-context-icon" aria-hidden="true">{item.icon}</span>
    <span className="composer-context-item-text"><span className="composer-context-label">{item.label}</span>
      {item.detail && <span className="composer-context-detail">{item.detail}</span>}
    </span>
  </button>

  return <>
    <button ref={anchorRef} type="button" className="icon-btn composer-add-btn composer-context-trigger"
      disabled={disabled} title={t('addContext')} aria-label={t('addContext')}
      aria-haspopup="menu" aria-expanded={visible} aria-controls={visible ? menuId : undefined}
      onMouseDown={event => event.preventDefault()}
      onKeyDown={event => {
        if (visible && event.key === 'Escape' && !event.nativeEvent.isComposing) {
          event.preventDefault(); event.stopPropagation(); close(true)
        }
      }}
      onClick={() => {
        if (disabled) return
        if (openRef.current) { close(); return }
        props.onOpen() // Snapshot editor selection before search steals focus.
        setQuery('')
        setActiveKey('upload')
        setFileError('')
        setSessionError(null)
        openRef.current = true
        setOpen(true)
      }}
    ><Plus size={15} weight="bold" /></button>
    <PanePopover anchorRef={anchorRef} positionAnchorRef={props.panelAnchorRef}
      matchAnchorWidth={Boolean(props.panelAnchorRef)} open={visible} onClose={dismiss} align="start" side="top"
      className="composer-context-popover" role="presentation">
      <div className="composer-context-panel" onKeyDown={onKeyDown}>
        <div className="composer-context-search">
          <MagnifyingGlass size={17} />
          <input ref={searchRef} type="search" value={query} disabled={pending !== null}
            placeholder={t('contextSearchPlaceholder')} aria-label={t('contextSearchPlaceholder')}
            aria-controls={menuId} aria-describedby={hintId}
            aria-activedescendant={pending === null && selectedKey ? itemId(selectedKey) : undefined}
            onCompositionStart={() => { composingRef.current = true }}
            onCompositionEnd={() => { composingRef.current = false }}
            onChange={event => {
              cancelSearch()
              setFiles([])
              setFileError('')
              setQuery(event.target.value)
              setActiveKey('upload')
            }}
          />
          {query && <button type="button" className="composer-context-small-button" disabled={pending !== null}
            aria-label={t('searchSessionsClear')} title={t('searchSessionsClear')}
            onClick={() => { cancelSearch(); setFiles([]); setQuery(''); setActiveKey('upload'); searchRef.current?.focus() }}
          ><X size={14} /></button>}
          <button type="button" className="composer-context-small-button" aria-label={t('contextClose')}
            title={t('contextClose')} onClick={() => close(true)}><X size={16} /></button>
        </div>
        <div ref={menuRef} id={menuId} className="composer-context-menu" role="menu" tabIndex={-1}
          aria-label={t('addContext')} aria-busy={pending !== null}>
          <div role="group" aria-labelledby={`${id}-add`} className="composer-context-group">
            <div id={`${id}-add`} className="composer-context-heading">{t('contextAddSection')}</div>
            {additions.filter(item => item.kind === 'upload').map(renderItem)}
          </div>
          {additions.some(item => item.kind === 'command') && <div role="group"
            aria-labelledby={`${id}-modes`} className="composer-context-group">
            <div id={`${id}-modes`} className="composer-context-heading">{t('contextModesSection')}</div>
            {additions.filter(item => item.kind === 'command').map(renderItem)}
          </div>}
          <div role="group" aria-labelledby={`${id}-files`} className="composer-context-group">
            <div id={`${id}-files`} className="composer-context-heading">{t('contextFilesSection')}</div>
            {!workspacePath || !filesAvailable ? <p className="composer-context-empty">{t('contextWorkspaceUnavailable')}</p>
              : loading ? <p className="composer-context-empty" role="status">{t('contextLoading')}</p>
                : fileError ? <><p className="composer-context-error" role="alert">{fileError}</p>{fileRetry && renderItem(fileRetry)}</>
                  : fileItems.length ? fileItems.map(renderItem) : <p className="composer-context-empty">{t('contextFilesEmpty')}</p>}
          </div>
          <div role="group" aria-labelledby={`${id}-sessions`} className="composer-context-group">
            <div id={`${id}-sessions`} className="composer-context-heading">{t('contextSessionsSection')}</div>
            {sessionError && <><p className="composer-context-error" role="alert">{sessionError.message}</p>
              {sessionRetry && renderItem(sessionRetry)}</>}
            {pending !== null && <p className="composer-context-empty" role="status">{t('contextLoading')}</p>}
            {sessionItems.length ? sessionItems.map(renderItem) : <p className="composer-context-empty">{t('contextSessionsEmpty')}</p>}
          </div>
        </div>
        <div id={hintId} className="composer-context-hint">
          <span>{t('contextSearchHint')}</span><span>{t('contextChooseHint')}</span>
        </div>
      </div>
    </PanePopover>
  </>
}

export default ComposerContextMenu
