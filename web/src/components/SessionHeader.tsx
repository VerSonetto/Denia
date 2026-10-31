import type { Dispatch, SetStateAction } from 'react';
import { t } from '../i18n';
import { sessionDisplayTitle } from '../sessionDisplay';
import type { SessionSummary, WorkspaceRecord } from '../types';
import { SessionPresetLabel } from './SessionPresetLabel';
import { OpenInApp } from './OpenInApp';
import { AgentPresetSelector } from './AgentPresetSelector';
import { IconCheck, IconChevron, IconDownload, IconPanelOpen, IconPlus, IconWorkspace } from './icons';
import type { ExportFormat } from '../lib/exportSession';
import type { AgentPresetsView } from '../api';
import * as api from '../api';
import { notify } from '../appStore';
import { downloadFile, serializeSession } from '../lib/exportSession';

export type ComposerView = 'chat' | 'trajectory'

/** 会话页头部:标题/预设标签、视图切换、导出与右侧面板开关。 */
export function SessionHeader({
  activeSession,
  activeId,
  agentPresetId,
  agentPresetCatalog,
  view,
  setView,
  sidePaneOpen,
  onToggleSidePane,
  exportOpen,
  exportBusy,
  setExportOpen,
  setExportBusy,
}: {
  activeSession: SessionSummary | null
  activeId: string | null
  agentPresetId: string | null
  agentPresetCatalog: AgentPresetsView | null
  view: ComposerView
  setView: (view: ComposerView) => void
  sidePaneOpen: boolean
  onToggleSidePane?: () => void
  exportOpen: boolean
  exportBusy: boolean
  setExportOpen: Dispatch<SetStateAction<boolean>>
  setExportBusy: Dispatch<SetStateAction<boolean>>
}) {
  /* ---- 导出会话:从后端拉全量 events 后回放文件 ---- */
  const handleExport = async (format: ExportFormat) => {
    if (!activeId || exportBusy) return
    setExportOpen(false)
    setExportBusy(true)
    try {
      const snapshot = await api.getSession(activeId)
      if (snapshot.events.length === 0) {
        notify('err', t('exportEmpty'))
        return
      }
      const title = activeSession ? sessionDisplayTitle(activeSession) : t('blankSession')
      const payload = serializeSession(format, {
        header: snapshot.header,
        events: snapshot.events,
        title,
      })
      downloadFile(payload.content, payload.filename, payload.mimeType)
      notify('ok', t('exportDownloaded', { name: payload.filename }))
    } catch (error) {
      console.error('[export] 会话导出失败', error)
      notify('err', t('exportFailed'))
    } finally {
      setExportBusy(false)
    }
  }

  return (
    <header className="session-header">
      {/* 标题块:标题与预设标签同处一块,标签紧贴标题文字。
          这样头部动作区一个元素都不新增,标签也不会飘到最右。 */}
      <div className="session-title-block">
        <h1 className="session-title">
          {activeSession ? sessionDisplayTitle(activeSession) : t('blankSession')}
        </h1>
        {activeSession && agentPresetId && (
          <SessionPresetLabel
            presetId={agentPresetId}
            presets={agentPresetCatalog?.presets ?? []}
          />
        )}
      </div>
      {activeSession?.cwd_alive === false && (
        <span className="badge err" title={t('deadCwdHint')}>
          {t('deadCwd')}
        </span>
      )}
      <div className="view-switch" role="tablist" aria-label={t('viewTrajectory')}>
        <button
          type="button"
          role="tab"
          aria-selected={view === 'chat'}
          className={`view-switch-tab${view === 'chat' ? ' active' : ''}`}
          onClick={() => setView('chat')}
        >
          {t('viewChat')}
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={view === 'trajectory'}
          className={`view-switch-tab${view === 'trajectory' ? ' active' : ''}`}
          onClick={() => setView('trajectory')}
        >
          {t('viewTrajectory')}
        </button>
      </div>
      <OpenInApp
        cwd={activeSession?.cwd_alive === false ? null : activeSession?.cwd ?? null}
      />
      {/* 右侧面板开关:工作区头部最右、紧邻导出。 */}
      {onToggleSidePane && (
        <button
          type="button"
          className={`icon-btn session-pane-btn${sidePaneOpen ? ' active' : ''}`}
          title={t('sidePaneToggle')}
          aria-label={sidePaneOpen ? t('sidePaneCollapse') : t('sidePaneExpand')}
          aria-pressed={sidePaneOpen}
          data-testid="side-pane-toggle"
          onClick={onToggleSidePane}
        >
          <IconPanelOpen size={15} />
        </button>
      )}
      <div className={`session-export${exportOpen ? ' open' : ''}`}>
        <button
          type="button"
          className="icon-btn session-export-btn"
          title={t('exportSessionHint')}
          aria-label={t('exportSession')}
          aria-haspopup="menu"
          aria-expanded={exportOpen}
          disabled={!activeId || exportBusy}
          onClick={() => setExportOpen((open) => !open)}
        >
          <IconDownload size={15} />
        </button>
        {exportOpen && (
          <>
            <div className="menu-backdrop" onClick={() => setExportOpen(false)} />
            <div className="session-export-menu" role="menu">
              <button
                type="button"
                role="menuitem"
                className="session-export-item"
                onClick={() => void handleExport('markdown')}
              >
                <span className="session-export-item-title">{t('exportAsMarkdown')}</span>
                <span className="session-export-item-hint">.md</span>
              </button>
              <button
                type="button"
                role="menuitem"
                className="session-export-item"
                onClick={() => void handleExport('json')}
              >
                <span className="session-export-item-title">{t('exportAsJson')}</span>
                <span className="session-export-item-hint">.json</span>
              </button>
            </div>
          </>
        )}
      </div>
    </header>
  )
}

/** 欢迎页(hero)的工作区选择行:工作区芯片 + 预设选择器 + 工作区菜单。 */
export function HeroWorkspaceRow({
  workspaces,
  activeWs,
  locked,
  inert,
  wsMenuOpen,
  setWsMenuOpen,
  agentPresetCatalog,
  agentPresetId,
  agentPresetBusy,
  changeAgentPreset,
  onSelectWorkspace,
  onAddWorkspace,
  onOpenPicker,
}: {
  workspaces: WorkspaceRecord[]
  activeWs: WorkspaceRecord | null
  locked: boolean
  inert: boolean
  wsMenuOpen: boolean
  setWsMenuOpen: Dispatch<SetStateAction<boolean>>
  agentPresetCatalog: AgentPresetsView | null
  agentPresetId: string | null
  agentPresetBusy: boolean
  changeAgentPreset: (id: string) => void
  onSelectWorkspace: (ws: WorkspaceRecord) => void
  onAddWorkspace: () => void
  onOpenPicker: () => void
}) {
  return (
    <div className="composer-workspace hero-ws-row">
      <button
        type="button"
        className={`ws-chip${wsMenuOpen ? ' open' : ''}`}
        aria-expanded={wsMenuOpen}
        onClick={() => {
          if (workspaces.length === 0) onOpenPicker()
          else setWsMenuOpen((open) => !open)
        }}
      >
        <IconWorkspace size={16} />
        <span className={activeWs ? 'label' : 'label dim'}>
          {activeWs?.title ?? t('heroChooseWorkspace')}
        </span>
        <IconChevron size={11} />
      </button>
      {/* 模式选择关闭时新会话固定用部署默认预设,选择器整个不渲染。 */}
      {agentPresetCatalog && agentPresetCatalog.modeSelection && agentPresetId && (
        <AgentPresetSelector
          value={agentPresetId}
          presets={agentPresetCatalog.presets}
          defaultId={agentPresetCatalog.default}
          disabled={inert || agentPresetBusy}
          onChange={changeAgentPreset}
        />
      )}
      {wsMenuOpen && (
        <>
          <div className="menu-backdrop" onClick={() => setWsMenuOpen(false)} />
          <div className="ws-menu">
            {workspaces.map((ws) => {
              const notCurrent = ws.id !== activeWs?.id
              const blocked = locked && notCurrent
              return (
                <button
                  key={ws.id}
                  type="button"
                  className={`ws-menu-item${ws.id === activeWs?.id ? ' active' : ''}`}
                  title={blocked ? t('workspaceLockedTitle') : ws.path}
                  disabled={blocked}
                  onClick={() => {
                    setWsMenuOpen(false)
                    onSelectWorkspace(ws)
                  }}
                >
                  <IconWorkspace size={18} />
                  <span className="ws-menu-text">
                    <span className="name">{ws.title}</span>
                    <span className="ws-menu-path">{ws.path}</span>
                  </span>
                  {ws.id === activeWs?.id && <IconCheck size={13} />}
                </button>
              )
            })}
            <button
              type="button"
              className="ws-menu-item create"
              onClick={() => {
                setWsMenuOpen(false)
                onAddWorkspace()
              }}
            >
              <IconPlus size={13} />
              <span className="name">{t('addWorkspaceMenu')}</span>
            </button>
          </div>
        </>
      )}
    </div>
  )
}
