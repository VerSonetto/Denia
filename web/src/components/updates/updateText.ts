import { getLocale } from '../../i18n'
import packageInfo from '../../../package.json'
import type { UpdateSource } from '../../updateApi'

export const installedVersion = packageInfo.version

const zh = {
  pageTitle: '版本与更新',
  back: '返回',
  edition: 'DESKTOP / 桌面版',
  currentVersion: '当前版本',
  versionUnknown: '正在读取版本',
  introduction: '查看当前版本、发布说明与安装进度。',
  softwareUpdate: '软件更新',
  close: '关闭更新对话框',
  unknownTitle: '检查 Denia 的最新版本',
  unknownDetail: '版本信息会在检查完成后显示。',
  checkingTitle: '正在检查更新',
  checkingDetail: '正在从 GitHub Releases 获取最新发布信息。',
  latestTitle: '已是最新版本',
  latestDetail: '当前版本无需更新，可以继续安心使用。',
  noReleaseTitle: '暂时没有可用的发布版本',
  noReleaseDetail: '发布仓库还没有可用版本，请稍后再检查。',
  availableTitle: '新版本已就绪',
  availableDetail: '查看更新说明，准备好后即可下载并安装。',
  noInstallerTitle: '新版本已发布，安装包尚未就绪',
  noInstallerDetail: '此版本没有兼容的 Windows 安装包。请稍后重试，或前往发布页面查看。',
  remoteTitle: '请在主机上安装更新',
  remoteDetail: '此连接不允许启动安装器。请在运行 Denia 的主机上打开本地界面完成更新。',
  checkFailedTitle: '暂时无法检查更新',
  checkFailedDetail: '现有版本仍可正常使用，请检查网络后重试。',
  selectingTitle: '正在选择下载源',
  selectingDetail: '正在测速各个 GitHub 加速源，选出最快的一个开始下载。',
  sourceProbing: '正在测速可用的下载源…',
  downloadingTitle: '正在下载安装包',
  downloadingDetail: '你可以关闭此窗口，下载会继续进行。',
  verifyingTitle: '正在校验安装包',
  verifyingDetail: '下载完成，正在核对安装包的完整性。',
  launchingTitle: '正在启动安装器',
  launchingDetail: '请留意系统安装窗口。安装器接管时，Denia 可能会正常退出。',
  launchedTitle: '安装器已启动',
  launchedDetail: '请在安装窗口中完成更新。Denia 退出是更新过程的一部分，不代表失败。',
  installFailedTitle: '更新未能完成',
  installFailedDetail: '当前版本未受影响，可以重新下载安装。',
  reconnectDetail: '暂时无法读取更新进度。这不代表安装失败，安装任务可能仍在主机上继续。',
  check: '检查更新',
  checkAgain: '重新检查',
  checking: '检查中…',
  retry: '重试',
  reconnect: '重新连接',
  install: '下载并安装',
  retryInstall: '重新下载并安装',
  later: '稍后再说',
  done: '关闭',
  viewVersion: '版本与更新',
  viewUpdate: '查看更新',
  installationNotice: '确认后将自动下载并校验 GitHub 安装包，再启动安装器。请先保存正在进行的工作。',
  releaseNotes: '更新说明',
  latestRelease: '最新发布',
  noNotes: '此版本尚未提供更新说明。',
  notesPending: '检查完成后，这里会显示最新版本的更新说明。',
  published: '发布于',
  prerelease: '预发布',
  releasePage: '在 GitHub 查看发布',
  repository: 'GitHub 仓库',
  checkedAt: '上次检查',
  neverChecked: '尚未检查',
  installer: '安装包',
  progress: '下载进度',
  verifying: '完整性校验中',
  launchPending: '等待安装器',
  unavailable: '暂无安装包',
  badge: '有新版本',
  badgeLabel: 'Denia {version} 可用，打开更新对话框',
}

const en: Record<keyof typeof zh, string> = {
  pageTitle: 'Version & updates',
  back: 'Back',
  edition: 'DESKTOP EDITION',
  currentVersion: 'Installed version',
  versionUnknown: 'Reading version',
  introduction: 'Your installed version, release notes, and update progress.',
  softwareUpdate: 'Software update',
  close: 'Close update dialog',
  unknownTitle: 'Check for the latest Denia release',
  unknownDetail: 'Version information will appear after a successful check.',
  checkingTitle: 'Checking for updates',
  checkingDetail: 'Getting the latest release from GitHub Releases.',
  latestTitle: 'You’re up to date',
  latestDetail: 'No update is needed. You can keep working with this version.',
  noReleaseTitle: 'No release is available yet',
  noReleaseDetail: 'There are no available releases in the repository. Check again later.',
  availableTitle: 'A new version is ready',
  availableDetail: 'Read what’s changed, then download and install when you’re ready.',
  noInstallerTitle: 'New release, installer not yet available',
  noInstallerDetail: 'This release has no compatible Windows installer. Check again later or visit the release page.',
  remoteTitle: 'Install this update on the host',
  remoteDetail: 'This connection cannot start the installer. Open Denia locally on the host to update.',
  checkFailedTitle: 'Couldn’t check for updates',
  checkFailedDetail: 'Your installed version is unaffected. Check your connection and try again.',
  selectingTitle: 'Choosing a download source',
  selectingDetail: 'Benchmarking the GitHub mirrors, then downloading from the fastest one.',
  sourceProbing: 'Benchmarking available sources…',
  downloadingTitle: 'Downloading the installer',
  downloadingDetail: 'You can close this window. The download will continue in the background.',
  verifyingTitle: 'Verifying the installer',
  verifyingDetail: 'The download is complete. Checking the installer’s integrity.',
  launchingTitle: 'Starting the installer',
  launchingDetail: 'Look for the system installation window. Denia may exit normally when the installer takes over.',
  launchedTitle: 'The installer has started',
  launchedDetail: 'Finish the update in the installation window. Denia exiting is part of the update, not a failure.',
  installFailedTitle: 'The update couldn’t be completed',
  installFailedDetail: 'Your installed version is unaffected. You can download and install again.',
  reconnectDetail: 'Update progress is temporarily unavailable. This does not mean installation failed; the task may still be running on the host.',
  check: 'Check for updates',
  checkAgain: 'Check again',
  checking: 'Checking…',
  retry: 'Try again',
  reconnect: 'Reconnect',
  install: 'Download & install',
  retryInstall: 'Download & install again',
  later: 'Not now',
  done: 'Close',
  viewVersion: 'Version & updates',
  viewUpdate: 'View update',
  installationNotice: 'Confirmation downloads and verifies the GitHub installer, then starts it automatically. Save your work first.',
  releaseNotes: 'Release notes',
  latestRelease: 'Latest release',
  noNotes: 'No release notes were provided for this version.',
  notesPending: 'The latest release notes will appear here after checking for updates.',
  published: 'Published',
  prerelease: 'Pre-release',
  releasePage: 'View release on GitHub',
  repository: 'GitHub repository',
  checkedAt: 'Last checked',
  neverChecked: 'Not checked yet',
  installer: 'Installer',
  progress: 'Download progress',
  verifying: 'Verifying integrity',
  launchPending: 'Waiting for the installer',
  unavailable: 'No installer available',
  badge: 'Update available',
  badgeLabel: 'Denia {version} is available. Open the update dialog',
}

export function updateText() {
  return getLocale() === 'en' ? en : zh
}

export function formatUpdateBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '—'
  if (bytes < 1024) return `${Math.floor(bytes)} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let value = bytes / 1024
  let index = 0
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024
    index += 1
  }
  return `${new Intl.NumberFormat(getLocale() === 'en' ? 'en-US' : 'zh-CN', { maximumFractionDigits: 1 }).format(value)} ${units[index]}`
}

export function formatUpdateSource(source: UpdateSource): string {
  const en = getLocale() === 'en'
  const name = source.direct
    ? (en ? 'GitHub direct' : 'GitHub 官方直连')
    : (en ? `Mirror ${source.label}` : `加速源 ${source.label}`)
  if (source.latencyMs === null || !Number.isFinite(source.latencyMs)) return name
  return `${name} · ${Math.max(0, Math.round(source.latencyMs))} ms`
}

export function formatUpdateDate(value: number | string): string | null {
  const date = new Date(value)
  if (!Number.isFinite(date.getTime())) return null
  return new Intl.DateTimeFormat(getLocale() === 'en' ? 'en-US' : 'zh-CN', {
    year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit',
  }).format(date)
}

export function safeUpdateUrl(value: string | undefined): string | undefined {
  if (!value) return undefined
  try {
    const url = new URL(value)
    return url.protocol === 'https:' ? url.href : undefined
  } catch {
    return undefined
  }
}
