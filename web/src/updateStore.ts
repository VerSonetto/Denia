import { useSyncExternalStore } from 'react'
import {
  fetchUpdateCheck, fetchUpdateStatus, requestUpdateInstall, UpdateApiError,
  type UpdateCheck, type UpdateStatus,
} from './updateApi'

export type { UpdateCheck, UpdateRelease, UpdateSource, UpdateStatus } from './updateApi'

export interface UpdateState {
  check: UpdateCheck | null
  checking: boolean
  checkError: string | null
  status: UpdateStatus
  dialogOpen: boolean
}

const idleStatus: UpdateStatus = {
  phase: 'idle', version: null, downloadedBytes: 0, totalBytes: null, error: null, source: null,
}
let state: UpdateState = {
  check: null, checking: false, checkError: null, status: idleStatus, dialogOpen: false,
}
const listeners = new Set<() => void>()
let checkFlight: Promise<void> | null = null
let installFlight: Promise<void> | null = null
let statusFlight: Promise<void> | null = null
let pollTimer: ReturnType<typeof globalThis.setTimeout> | undefined
let startupChecked = false
let pageLeaving = false
let lifecycleBound = false
let statusGeneration = 0

function update(patch: Partial<UpdateState>): void {
  state = { ...state, ...patch }
  for (const listener of listeners) listener()
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => void listeners.delete(listener)
}

export function getUpdateState(): UpdateState {
  return state
}

export function useUpdateState(): UpdateState {
  return useSyncExternalStore(subscribe, getUpdateState, getUpdateState)
}

export function useUpdateDialogOpen(): boolean {
  return useSyncExternalStore(subscribe, () => state.dialogOpen, () => false)
}

export function isUpdateBusy(status: UpdateStatus): boolean {
  return status.phase === 'selecting' || status.phase === 'downloading'
    || status.phase === 'verifying' || status.phase === 'launching'
}

function clearPoll(): void {
  if (pollTimer !== undefined) globalThis.clearTimeout(pollTimer)
  pollTimer = undefined
}

function bindLifecycle(): void {
  if (lifecycleBound || typeof window === 'undefined') return
  lifecycleBound = true
  window.addEventListener('pagehide', () => {
    pageLeaving = true
    clearPoll()
  })
  window.addEventListener('pageshow', () => {
    pageLeaving = false
    if (isUpdateBusy(state.status)) void refreshUpdateStatus()
  })
}

function schedulePoll(): void {
  clearPoll()
  if (pageLeaving || !isUpdateBusy(state.status) || state.status.error) return
  pollTimer = globalThis.setTimeout(() => {
    pollTimer = undefined
    void refreshUpdateStatus()
  }, 800)
}

function acceptStatus(status: UpdateStatus): void {
  update({ status })
  schedulePoll()
}

export function refreshUpdateStatus(): Promise<void> {
  if (statusFlight) return statusFlight
  clearPoll()
  const generation = statusGeneration
  statusFlight = (async () => {
    try {
      const status = await fetchUpdateStatus()
      if (!pageLeaving && generation === statusGeneration) acceptStatus(status)
    } catch (error) {
      // Losing the server during installer handoff is not an installation failure.
      if (!pageLeaving && generation === statusGeneration && isUpdateBusy(state.status)) {
        update({ status: { ...state.status, error: (error as Error).message } })
      }
    } finally {
      statusFlight = null
      schedulePoll()
    }
  })()
  return statusFlight
}

export function checkForUpdates(force = false): Promise<void> {
  if (checkFlight) return checkFlight
  update({ checking: true, checkError: null })
  checkFlight = (async () => {
    try {
      update({ check: await fetchUpdateCheck(force) })
    } catch (error) {
      update({ checkError: (error as Error).message })
    } finally {
      checkFlight = null
      update({ checking: false })
    }
  })()
  return checkFlight
}

export function ensureStartupUpdateCheck(): void {
  if (startupChecked) return
  startupChecked = true
  bindLifecycle()
  void checkForUpdates()
  void refreshUpdateStatus()
}

export function openUpdateDialog(forceCheck = false): void {
  bindLifecycle()
  update({ dialogOpen: true })
  void refreshUpdateStatus()
  if (forceCheck || !state.check || state.checkError) void checkForUpdates(forceCheck)
}

export function closeUpdateDialog(): void {
  update({ dialogOpen: false })
}

export function installUpdate(): Promise<void> {
  if (installFlight) return installFlight
  if (isUpdateBusy(state.status)) return refreshUpdateStatus()
  if (state.status.phase === 'launched') return Promise.resolve()
  const check = state.check
  if (!check?.updateAvailable || !check.canInstall || !check.latest?.installer) return Promise.resolve()
  bindLifecycle()
  const version = check.latest.version
  statusGeneration += 1
  update({
    status: { phase: 'selecting', version, downloadedBytes: 0, totalBytes: check.latest.installer.size || null, error: null, source: null },
  })
  installFlight = (async () => {
    try {
      acceptStatus(await requestUpdateInstall(version))
    } catch (error) {
      if (pageLeaving) return
      if (error instanceof UpdateApiError && error.status > 0) {
        acceptStatus({ ...idleStatus, phase: 'failed', version, error: error.message })
        return
      }
      // A timed-out POST may already have started the job. Reconcile before offering a new POST.
      try {
        const status = await fetchUpdateStatus()
        acceptStatus(status.phase === 'idle'
          ? { ...idleStatus, phase: 'failed', version, error: (error as Error).message }
          : status)
      } catch {
        update({ status: { ...state.status, error: (error as Error).message } })
      }
    } finally {
      installFlight = null
    }
  })()
  return installFlight
}
