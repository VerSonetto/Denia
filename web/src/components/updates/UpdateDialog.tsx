import { useEffect, useId, useRef } from 'react'
import { createPortal } from 'react-dom'
import {
  checkForUpdates, closeUpdateDialog, installUpdate, isUpdateBusy, useUpdateState,
} from '../../updateStore'
import { IconClose, IconDownload } from '../icons'
import { ReleaseNotes } from './ReleaseNotes'
import { UpdateSummary } from './UpdateSummary'
import { installedVersion, updateText } from './updateText'
import './updates.css'

function focusableElements(container: HTMLElement): HTMLElement[] {
  return [...container.querySelectorAll<HTMLElement>('button, a[href], input, select, textarea, [tabindex]')]
    .filter((element) => element.tabIndex >= 0 && !element.matches(':disabled')
      && !element.closest('[hidden], [inert], [aria-hidden="true"]')
      && getComputedStyle(element).display !== 'none' && getComputedStyle(element).visibility !== 'hidden')
}

export function UpdateDialog({ onOpenVersion }: { onOpenVersion?: () => void } = {}) {
  const state = useUpdateState()
  const text = updateText()
  const titleId = useId()
  const descriptionId = useId()
  const backdropRef = useRef<HTMLDivElement>(null)
  const dialogRef = useRef<HTMLDivElement>(null)
  const { dialogOpen, check, checking, checkError, status } = state
  const busy = isUpdateBusy(status)
  const canInstall = !!(check?.updateAvailable && check.canInstall && check.latest?.installer)
  const offerInstall = canInstall && !checking && !checkError && !busy && status.phase !== 'launched'

  useEffect(() => {
    if (!dialogOpen) return
    const dialog = dialogRef.current
    const backdrop = backdropRef.current
    if (!dialog || !backdrop) return
    const previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null
    const previousOverflow = document.body.style.overflow
    dialog.focus()
    const background = [...document.body.children]
      .filter((element): element is HTMLElement => element instanceof HTMLElement && element !== backdrop)
      .map((element) => ({ element, inert: element.inert, ariaHidden: element.getAttribute('aria-hidden') }))
    for (const { element } of background) {
      element.inert = true
      element.setAttribute('aria-hidden', 'true')
    }
    document.body.style.overflow = 'hidden'
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault()
        event.stopImmediatePropagation()
        closeUpdateDialog()
      } else if (event.key === 'Tab') {
        const elements = focusableElements(dialog)
        const first = elements[0]
        const last = elements[elements.length - 1]
        if (!first || !last) {
          event.preventDefault()
          dialog.focus()
        } else if (event.shiftKey && (document.activeElement === first || document.activeElement === dialog)) {
          event.preventDefault()
          last.focus()
        } else if (!event.shiftKey && (document.activeElement === last || document.activeElement === dialog)) {
          event.preventDefault()
          first.focus()
        }
      }
    }
    const onFocus = (event: FocusEvent) => {
      if (event.target instanceof Node && !dialog.contains(event.target)) dialog.focus()
    }
    document.addEventListener('keydown', onKeyDown, true)
    document.addEventListener('focusin', onFocus)
    return () => {
      document.removeEventListener('keydown', onKeyDown, true)
      document.removeEventListener('focusin', onFocus)
      for (const { element, inert, ariaHidden } of background) {
        element.inert = inert
        if (ariaHidden === null) element.removeAttribute('aria-hidden')
        else element.setAttribute('aria-hidden', ariaHidden)
      }
      document.body.style.overflow = previousOverflow
      if (previousFocus?.isConnected) previousFocus.focus()
    }
  }, [dialogOpen])

  if (!dialogOpen) return null
  return createPortal(
    <div className="update-backdrop" ref={backdropRef} onMouseDown={(event) => {
      if (event.target === event.currentTarget) closeUpdateDialog()
    }}>
      <div
        className="update-dialog update-scope"
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        aria-describedby={descriptionId}
        tabIndex={-1}
      >
        <header className="update-dialog-head">
          <div>
            <span className="update-eyebrow">DENIA / RELEASES</span>
            <h1 id={titleId}>{text.softwareUpdate}</h1>
          </div>
          <button className="update-icon-button" type="button" aria-label={text.close} onClick={closeUpdateDialog}><IconClose size={18} /></button>
        </header>
        <div className="update-dialog-body">
          <div className="update-version-line" id={descriptionId}>
            <span>{text.currentVersion}</span>
            <strong>{`v${(check?.currentVersion || installedVersion).replace(/^v/, '')}`}</strong>
            {check?.updateAvailable && check.latest && <span className="update-version-next">/ v{check.latest.version.replace(/^v/, '')}</span>}
          </div>
          <UpdateSummary state={state} />
          {check?.latest && <ReleaseNotes release={check.latest} />}
          {offerInstall && <p className="update-install-notice">{text.installationNotice}</p>}
        </div>
        <footer className="update-dialog-footer">
          {onOpenVersion && (
            <button className="update-text-button update-view-version" type="button" onClick={() => {
              closeUpdateDialog()
              onOpenVersion()
            }}>{text.viewVersion}</button>
          )}
          <div className="update-dialog-actions">
            <button className="update-button update-button-secondary" type="button" onClick={closeUpdateDialog}>
              {status.phase === 'launched' || busy ? text.done : text.later}
            </button>
            {busy && status.error ? (
              <button className="update-button update-button-primary" type="button" onClick={() => void installUpdate()}>{text.reconnect}</button>
            ) : offerInstall ? (
              <button className="update-button update-button-primary" type="button" onClick={() => void installUpdate()}>
                <IconDownload size={15} />{status.phase === 'failed' ? text.retryInstall : text.install}
              </button>
            ) : !busy && status.phase !== 'launched' ? (
              <button className="update-button update-button-primary" type="button" disabled={checking} onClick={() => void checkForUpdates(true)}>
                {checking ? text.checking : checkError ? text.retry : text.checkAgain}
              </button>
            ) : null}
          </div>
        </footer>
      </div>
    </div>,
    document.body,
  )
}
