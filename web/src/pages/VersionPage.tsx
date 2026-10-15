import { useEffect, useRef } from 'react'
import { DeniaWordmark } from '../components/DeniaWordmark'
import { IconArrowLeft, IconRefresh, IconUpdate } from '../components/icons'
import { ReleaseNotes } from '../components/updates/ReleaseNotes'
import { UpdateSummary } from '../components/updates/UpdateSummary'
import { formatUpdateDate, installedVersion, safeUpdateUrl, updateText } from '../components/updates/updateText'
import { ensureStartupUpdateCheck, isUpdateBusy, openUpdateDialog, useUpdateState } from '../updateStore'
import '../components/updates/updates.css'

export default function VersionPage({ onBack }: { onBack: () => void }) {
  const state = useUpdateState()
  const text = updateText()
  const titleRef = useRef<HTMLHeadingElement>(null)
  const { check, checking, status } = state
  const busy = isUpdateBusy(status)
  const checkedAt = check?.checkedAt ? formatUpdateDate(check.checkedAt) : null
  const repository = safeUpdateUrl(check?.repositoryUrl)
  const hasUpdate = !!(check?.updateAvailable && check.latest)
  const currentVersion = check?.currentVersion || installedVersion

  useEffect(() => {
    ensureStartupUpdateCheck()
    titleRef.current?.focus({ preventScroll: true })
  }, [])

  return (
    <section className="version-page update-scope" aria-labelledby="version-page-title">
      <div className="version-page-inner">
        <header className="version-page-header">
          <button type="button" className="update-back-button" onClick={onBack}><IconArrowLeft size={17} /><span>{text.back}</span></button>
          <h1 id="version-page-title" ref={titleRef} tabIndex={-1}>{text.pageTitle}</h1>
          <span className="update-eyebrow version-header-edition">{text.edition}</span>
        </header>

        <section className="version-hero" aria-label={text.currentVersion}>
          <div className="version-brand">
            <DeniaWordmark />
            <p>{text.introduction}</p>
          </div>
          <div className="version-number-block">
            <span className="update-eyebrow">{text.currentVersion}</span>
            <p className="version-number" aria-label={`${text.currentVersion} ${currentVersion}`}>
              <span className="version-v">v</span>{currentVersion.replace(/^v/, '')}
            </p>
          </div>
        </section>

        <section className="version-update-section" aria-label={text.softwareUpdate}>
          <div className="version-update-topline"><span className="update-eyebrow">01 / {text.softwareUpdate}</span></div>
          <div className="version-update-content">
            <UpdateSummary state={state} />
            <div className="version-update-actions">
              {(hasUpdate || busy || status.phase === 'launched' || status.phase === 'failed') && (
                <button className="update-button update-button-primary" type="button" onClick={() => openUpdateDialog()}>
                  <IconUpdate size={16} />{busy || status.phase === 'launched' ? text.softwareUpdate : text.viewUpdate}
                </button>
              )}
              <button className={`update-button ${hasUpdate || busy ? 'update-button-secondary' : 'update-button-primary'}`} type="button" disabled={checking || busy} onClick={() => openUpdateDialog(true)}>
                <IconRefresh size={15} />{checking ? text.checking : text.check}
              </button>
              <p className="version-last-checked">{checkedAt ? `${text.checkedAt} ${checkedAt}` : text.neverChecked}</p>
            </div>
          </div>
        </section>

        <section className="version-notes-section">
          <span className="update-eyebrow">02 / {text.latestRelease}</span>
          <ReleaseNotes release={check?.latest ?? null} />
        </section>
        <footer className="version-page-footer">
          <span>Denia</span>
          {repository && <a href={repository} target="_blank" rel="noopener noreferrer">{text.repository}<span aria-hidden="true"> ↗</span></a>}
        </footer>
      </div>
    </section>
  )
}
