import { IconCheck, IconUpdate } from '../icons'
import { isUpdateBusy, type UpdateState } from '../../updateStore'
import { formatUpdateBytes, formatUpdateSource, updateText } from './updateText'

export function UpdateSummary({ state }: { state: UpdateState }) {
  const text = updateText()
  const { check, status, checking, checkError } = state
  const busy = isUpdateBusy(status)
  let title: string
  let detail: string
  let kind = 'neutral'
  if (busy || status.phase === 'launched' || status.phase === 'failed') {
    const phase = status.phase
    const messages = {
      selecting: [text.selectingTitle, text.selectingDetail],
      downloading: [text.downloadingTitle, text.downloadingDetail],
      verifying: [text.verifyingTitle, text.verifyingDetail],
      launching: [text.launchingTitle, text.launchingDetail],
      launched: [text.launchedTitle, text.launchedDetail],
      failed: [text.installFailedTitle, text.installFailedDetail],
    }
    const message = messages[phase as keyof typeof messages]
    title = message[0]
    detail = message[1]
    kind = phase === 'failed' ? 'error' : phase === 'launched' ? 'complete' : 'active'
  } else if (checking) {
    title = text.checkingTitle
    detail = text.checkingDetail
    kind = 'checking'
  } else if (checkError) {
    title = text.checkFailedTitle
    detail = text.checkFailedDetail
    kind = 'error'
  } else if (!check) {
    title = text.unknownTitle
    detail = text.unknownDetail
  } else if (!check.latest) {
    title = text.noReleaseTitle
    detail = text.noReleaseDetail
  } else if (!check.updateAvailable) {
    title = text.latestTitle
    detail = text.latestDetail
    kind = 'complete'
  } else if (!check.latest.installer) {
    title = text.noInstallerTitle
    detail = text.noInstallerDetail
  } else if (!check.canInstall) {
    title = text.remoteTitle
    detail = text.remoteDetail
  } else {
    title = text.availableTitle
    detail = text.availableDetail
    kind = 'available'
  }
  const error = status.phase === 'failed' || busy ? status.error : checkError
  const total = status.totalBytes && status.totalBytes > 0 ? status.totalBytes : null
  const percent = total ? Math.max(0, Math.min(100, status.downloadedBytes / total * 100)) : null
  return (
    <section className="update-summary" data-kind={kind} aria-busy={checking || busy}>
      <div className="update-summary-heading">
        <span className="update-status-mark" aria-hidden="true">
          {kind === 'complete' ? <IconCheck size={19} /> : <IconUpdate size={20} />}
        </span>
        <div className="update-status-copy" role="status" aria-live="polite" aria-atomic="true">
          <h2>{title}</h2>
          <p>{detail}</p>
        </div>
      </div>
      {status.phase === 'downloading' && (
        <div className="update-download">
          <div
            className="update-progress-track"
            role="progressbar"
            aria-label={text.progress}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={percent === null ? undefined : Math.round(percent)}
            aria-valuetext={`${formatUpdateBytes(status.downloadedBytes)}${total ? ` / ${formatUpdateBytes(total)}` : ''}`}
            data-indeterminate={percent === null || undefined}
          >
            <span style={{ width: percent === null ? '28%' : `${percent}%` }} />
          </div>
          <div className="update-download-labels">
            <span>{formatUpdateBytes(status.downloadedBytes)}{total ? ` / ${formatUpdateBytes(total)}` : ''}</span>
            {percent !== null && <span>{Math.floor(percent)}%</span>}
          </div>
        </div>
      )}
      {(status.phase === 'verifying' || status.phase === 'launching') && (
        <p className="update-stage-caption">
          {status.phase === 'verifying' ? text.verifying : text.launchPending}
          {status.source && ` · ${formatUpdateSource(status.source)}`}
        </p>
      )}
      {(status.phase === 'selecting' || status.phase === 'downloading') && (
        <p className="update-stage-caption">{status.source ? formatUpdateSource(status.source) : text.sourceProbing}</p>
      )}
      {error && (
        <div className="update-error" role={busy ? 'status' : 'alert'}>
          {busy && <p>{text.reconnectDetail}</p>}
          <p>{error}</p>
        </div>
      )}
    </section>
  )
}
