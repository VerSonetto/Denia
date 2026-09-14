/**
 * 会话头部的「用应用打开」分裂按钮:主按钮显示当前选中应用的真实图标,
 * 点击即用该应用打开会话工作区目录;旁侧 chevron 展开菜单列出 host 探测
 * 到的已安装应用。host 没探测到可命名应用、或会话没有工作区目录时整个
 * 控件不渲染。上次选择记在 localStorage,跨会话与刷新共享。
 */

import { useEffect, useRef, useState } from 'react'

import { getOpenInAppApps, openInAppOpen } from '../api'
import { t } from '../i18n'
import { IconChevronDown } from './icons'

type DictKey = Parameters<typeof t>[0]

/** 应用 id → 展示文案键;host 探测到但词典没有的 id 不展示(避免裸 id)。 */
const APP_LABELS: Record<string, DictKey> = {
  finder: 'openInAppAppFinder',
  explorer: 'openInAppAppExplorer',
  filemanager: 'openInAppAppFilemanager',
  cursor: 'openInAppAppCursor',
  vscode: 'openInAppAppVscode',
  vscodeinsiders: 'openInAppAppVscodeinsiders',
  windsurf: 'openInAppAppWindsurf',
  zed: 'openInAppAppZed',
  sublimetext: 'openInAppAppSublimetext',
  xcode: 'openInAppAppXcode',
  androidstudio: 'openInAppAppAndroidstudio',
  intellij: 'openInAppAppIntellij',
  pycharm: 'openInAppAppPycharm',
  webstorm: 'openInAppAppWebstorm',
  phpstorm: 'openInAppAppPhpstorm',
  goland: 'openInAppAppGoland',
  rider: 'openInAppAppRider',
  rustrover: 'openInAppAppRustrover',
  fork: 'openInAppAppFork',
  sourcetree: 'openInAppAppSourcetree',
  github: 'openInAppAppGithub',
  tower: 'openInAppAppTower',
  gitkraken: 'openInAppAppGitkraken',
  smartgit: 'openInAppAppSmartgit',
  sublimemerge: 'openInAppAppSublimemerge',
  ghostty: 'openInAppAppGhostty',
  warp: 'openInAppAppWarp',
  iterm: 'openInAppAppIterm',
  kitty: 'openInAppAppKitty',
  terminal: 'openInAppAppTerminal',
  windowsterminal: 'openInAppAppWindowsterminal',
  gitbash: 'openInAppAppGitbash',
  gnometerminal: 'openInAppAppGnometerminal',
  konsole: 'openInAppAppKonsole',
}

/** 上次选中的应用 id。 */
const CHOICE_KEY = 'denia.open-in-app.choice'

/** 可用应用每页只探测一次;失败按空表处理(控件隐藏)。 */
let appsCache: string[] | null = null
let appsPromise: Promise<string[]> | null = null

/** 图标已 404 的应用:本页只请求一次,之后直接画占位方块。 */
const failedIcons = new Set<string>()

/** 可用应用每页只探测一次,产物 chip 等其它调用方复用同一份缓存;失败按
 * 空表处理(控件隐藏)。 */
export function fetchApps(): Promise<string[]> {
  appsPromise ??= getOpenInAppApps()
    .then((data) => {
      appsCache = data.apps
      return data.apps
    })
    .catch(() => {
      appsCache = []
      return []
    })
  return appsPromise
}

/** 应用的真实图标(host 提取的 PNG),失败回落到内联的通用应用方块。 */
function AppIcon({ id, size }: { id: string; size: number }) {
  const [failed, setFailed] = useState(() => failedIcons.has(id))
  if (failed) {
    return (
      <svg
        width={size}
        height={size}
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth={1.8}
        className="open-in-app-icon"
        aria-hidden="true"
      >
        <rect x={3} y={3} width={18} height={18} rx={5} />
      </svg>
    )
  }
  return (
    <img
      src={`/api/open-in-app/icon/${id}`}
      width={size}
      height={size}
      className="open-in-app-icon"
      alt=""
      aria-hidden="true"
      draggable={false}
      onError={() => {
        failedIcons.add(id)
        setFailed(true)
      }}
    />
  )
}

type Phase = 'idle' | 'busy' | 'error'

/** 快速启动在此时限内完成就不闪 busy 态,dim-and-wait 只留给真慢的启动。 */
const BUSY_DRESS_DELAY_MS = 250

/** 错误态停留时长;到点自动回落,不赖在按钮上。 */
const ERROR_DECAY_MS = 2000

export function OpenInApp({ cwd }: { cwd: string | null }) {
  const [apps, setApps] = useState<string[] | null>(appsCache)
  const [choice, setChoice] = useState(() => localStorage.getItem(CHOICE_KEY) ?? '')
  const [open, setOpen] = useState(false)
  const [phase, setPhase] = useState<Phase>('idle')
  const inFlight = useRef(false)
  const busyTimer = useRef<ReturnType<typeof setTimeout>>(undefined)
  const errorTimer = useRef<ReturnType<typeof setTimeout>>(undefined)

  useEffect(() => {
    if (appsCache === null) {
      void fetchApps().then(setApps)
    }
    return () => {
      clearTimeout(busyTimer.current)
      clearTimeout(errorTimer.current)
    }
  }, [])

  const entries = (apps ?? [])
    .map((id) => ({ id, labelKey: APP_LABELS[id] }))
    .filter((entry): entry is { id: string; labelKey: DictKey } => entry.labelKey !== undefined)
  const currentEntry = entries.find((entry) => entry.id === choice) ?? entries[0]
  if (currentEntry === undefined || cwd === null || cwd === '') return null

  const currentLabel = t(currentEntry.labelKey)
  const title = phase === 'error' ? t('openInAppError') : t('openInAppOpen', { app: currentLabel })

  const launch = (appId: string) => {
    if (inFlight.current) return
    inFlight.current = true
    // 在途的 error 衰减不能把按钮中途翻回 idle。
    clearTimeout(errorTimer.current)
    clearTimeout(busyTimer.current)
    busyTimer.current = setTimeout(() => setPhase('busy'), BUSY_DRESS_DELAY_MS)
    openInAppOpen(appId, cwd).then(
      () => {
        inFlight.current = false
        clearTimeout(busyTimer.current)
        setPhase('idle')
      },
      () => {
        inFlight.current = false
        clearTimeout(busyTimer.current)
        setPhase('error')
        clearTimeout(errorTimer.current)
        errorTimer.current = setTimeout(() => setPhase('idle'), ERROR_DECAY_MS)
      },
    )
  }

  return (
    <div className={`open-in-app${open ? ' open' : ''}`}>
      {open && <div className="menu-backdrop" onClick={() => setOpen(false)} />}
      <div className="open-in-app-split" role="group" aria-label={t('openInAppMenu')}>
        <button
          type="button"
          className="open-in-app-main"
          data-state={phase}
          disabled={phase === 'busy'}
          aria-label={title}
          title={title}
          onClick={() => launch(currentEntry.id)}
        >
          <AppIcon id={currentEntry.id} size={15} />
        </button>
        <button
          type="button"
          className="open-in-app-chevron"
          aria-expanded={open}
          aria-haspopup="menu"
          aria-label={t('openInAppMenu')}
          title={t('openInAppMenu')}
          onClick={() => setOpen((value) => !value)}
        >
          <IconChevronDown size={12} />
        </button>
      </div>
      {open && (
        <div className="open-in-app-menu" role="menu">
          {entries.map((entry) => (
            <button
              type="button"
              key={entry.id}
              role="menuitemradio"
              aria-checked={entry.id === currentEntry.id}
              className={`open-in-app-item${entry.id === currentEntry.id ? ' active' : ''}`}
              onClick={() => {
                setOpen(false)
                // 在途启动时整条忽略:只记选择不启动,会让按钮说出
                // 一个这次手势根本没打开的应用。
                if (inFlight.current) return
                localStorage.setItem(CHOICE_KEY, entry.id)
                setChoice(entry.id)
                launch(entry.id)
              }}
            >
              <span className="open-in-app-item-icon">
                <AppIcon id={entry.id} size={18} />
              </span>
              <span className="open-in-app-item-name">{t(entry.labelKey)}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  )
}
