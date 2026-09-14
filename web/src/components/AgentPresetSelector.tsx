import { useEffect, useState } from 'react'
import * as api from '../api'
import { t } from '../i18n'
import { IconAgentPreset, IconCheck, IconChevron } from './icons'

/**
 * agent preset(组装)选择器:新会话页里与工作区 chip 同排的那一个。
 *
 * 组装决定模型看到的工具面与 persona,而会话一旦开始工作就固定下来——所以
 * 它只长在"还没发出第一条消息"的那一屏(工作区行),会话进行后没有任何
 * 改选入口(服务端对已产出内容的会话返回 `agent-preset/locked`)。
 */
export function AgentPresetSelector({
  value,
  presets,
  defaultId,
  onChange,
  disabled,
}: {
  /** 会话当前运行的 preset id。 */
  value: string
  presets: api.AgentPresetRow[]
  /** 设置里的默认 preset id。 */
  defaultId: string
  onChange: (id: string) => void
  /** 输入栏 inert(无工作区)或切换请求在途。 */
  disabled?: boolean
}) {
  const [open, setOpen] = useState(false)

  useEffect(() => {
    if (!open) return
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setOpen(false)
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [open])

  const active = presets.find((row) => row.id === value)
  const label = active?.name ?? value

  return (
    <div className={`preset-selector${open ? ' open' : ''}`}>
      <button
        type="button"
        className={`ws-chip${open ? ' open' : ''}`}
        aria-expanded={open}
        aria-haspopup="menu"
        disabled={disabled}
        title={t('agentPresetLabel')}
        onClick={() => setOpen(!open)}
      >
        <IconAgentPreset size={13} />
        <span className="preset-chip-name">{label}</span>
        <IconChevron size={11} />
      </button>
      {open && (
        <>
          <div className="menu-backdrop" onClick={() => setOpen(false)} />
          <div
            className="permission-menu preset-menu"
            role="menu"
            aria-label={t('agentPresetLabel')}
          >
            <div className="permission-menu-heading">{t('agentPresetLabel')}</div>
            {presets.map((preset) => {
              const selected = preset.id === value
              return (
                <button
                  key={`${preset.trust}:${preset.id}`}
                  type="button"
                  role="menuitemradio"
                  aria-checked={selected}
                  disabled={Boolean(preset.broken)}
                  className={`permission-item${selected ? ' active' : ''}`}
                  onClick={() => {
                    if (preset.id !== value) onChange(preset.id)
                    setOpen(false)
                  }}
                >
                  <span className="permission-item-icon">
                    <IconAgentPreset size={14} />
                  </span>
                  <span className="permission-item-text">
                    <span className="permission-item-label">
                      {preset.name}
                      {preset.id === defaultId && (
                        <span className="preset-badge">{t('agentPresetDefaultBadge')}</span>
                      )}
                    </span>
                    <span className="permission-item-desc">
                      {preset.broken
                        ? t('agentPresetBrokenReason', { reason: preset.broken })
                        : preset.description}
                    </span>
                  </span>
                  {selected && (
                    <span className="permission-item-check">
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
  )
}
