import type { PastedImage, PendingAttachment, QueuedMessage } from './types'
import { isPendingAttachment } from './types'
import type { TrajectoryQuote } from './trajectory'

export interface ComposerSnapshot {
  text: string
  images: PastedImage[]
  attachments: PendingAttachment[]
  quotes: TrajectoryQuote[]
  queue: QueuedMessage[]
}

interface StoredComposer {
  snapshot: ComposerSnapshot
  revision: number
}

const databaseName = 'denia-composer-memory'
const storeName = 'composers'
const textPrefix = 'denia:composer-text:'
const writes = new Map<string, Promise<void>>()
let lastRevision = 0

function checkScope(scope: string) {
  if (!/^(session|workspace):.+$/s.test(scope)) throw new Error('Invalid composer scope')
}

function emptyComposer(): ComposerSnapshot {
  return { text: '', images: [], attachments: [], quotes: [], queue: [] }
}

function readTextBackup(scope: string): { text: string; revision: number } | null {
  checkScope(scope)
  const raw = localStorage.getItem(textPrefix + scope)
  if (raw === null) return null
  const value = JSON.parse(raw)
  if (!value || typeof value.text !== 'string' || !Number.isFinite(value.revision)) {
    throw new Error('Invalid composer text backup')
  }
  return value
}

export function readComposerText(scope: string): string | null {
  return readTextBackup(scope)?.text ?? null
}

function openDatabase(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(databaseName, 1)
    let blocked = false
    request.onupgradeneeded = () => {
      if (!request.result.objectStoreNames.contains(storeName)) request.result.createObjectStore(storeName)
    }
    request.onerror = () => reject(request.error ?? new Error('Cannot open composer storage'))
    request.onblocked = () => {
      blocked = true
      reject(new Error('Composer storage upgrade is blocked by another tab'))
    }
    request.onsuccess = () => {
      if (blocked) request.result.close()
      else resolve(request.result)
    }
  })
}

async function accessStore<T>(mode: IDBTransactionMode, action: (store: IDBObjectStore) => IDBRequest<T>): Promise<T> {
  const database = await openDatabase()
  try {
    return await new Promise<T>((resolve, reject) => {
      const transaction = database.transaction(storeName, mode)
      const request = action(transaction.objectStore(storeName))
      transaction.oncomplete = () => resolve(request.result)
      transaction.onabort = () => reject(transaction.error ?? request.error ?? new Error('Composer storage transaction aborted'))
      transaction.onerror = () => reject(transaction.error ?? request.error ?? new Error('Composer storage transaction failed'))
    })
  } finally {
    database.close()
  }
}

function validateSnapshot(snapshot: ComposerSnapshot) {
  if (!snapshot || typeof snapshot.text !== 'string'
    || !Array.isArray(snapshot.images) || !Array.isArray(snapshot.attachments)
    || !Array.isArray(snapshot.quotes) || !Array.isArray(snapshot.queue)) {
    throw new Error('Invalid composer snapshot')
  }
  // 草稿来自 IndexedDB，旧版本存的是 File 句柄（刷新后本来也拿不到内容）。
  // 恢复时只保留带绝对路径的附件：发出去才算数，宁可少显示也不要在发送时才失败。
  // 队列里的排队消息带同一套载荷，同样要洗一遍。
  snapshot.attachments = snapshot.attachments.filter(isPendingAttachment)
  for (const message of snapshot.queue) {
    if (Array.isArray(message.attachments)) {
      message.attachments = message.attachments.filter(isPendingAttachment)
    }
  }
}

export async function loadComposer(scope: string): Promise<ComposerSnapshot> {
  checkScope(scope)
  await writes.get(scope)?.catch(() => {})
  const stored = await accessStore<StoredComposer | undefined>('readonly', (store) => store.get(scope))
  const snapshot = stored === undefined ? emptyComposer() : stored.snapshot
  validateSnapshot(snapshot)
  if (stored && !Number.isFinite(stored.revision)) throw new Error('Invalid composer revision')
  const backup = readTextBackup(scope)
  if (backup && (!stored || backup.revision >= stored.revision)) snapshot.text = backup.text
  return snapshot
}

export function saveComposer(scope: string, snapshot: ComposerSnapshot): Promise<void> {
  let stored: StoredComposer
  let backupError: unknown
  try {
    checkScope(scope)
    validateSnapshot(snapshot)
    lastRevision = Math.max(Date.now(), lastRevision + 1)
    // Capture before waiting for earlier writes; callers may reuse their arrays.
    stored = { snapshot: structuredClone(snapshot), revision: lastRevision }
  } catch (error) {
    return Promise.reject(error)
  }
  try {
    localStorage.setItem(textPrefix + scope, JSON.stringify({ text: stored.snapshot.text, revision: stored.revision }))
  } catch (error) {
    backupError = error
  }
  const write = (writes.get(scope) ?? Promise.resolve()).catch(() => {}).then(async () => {
    try {
      await accessStore('readwrite', (store) => store.put(stored, scope))
    } catch (error) {
      if (backupError) throw new AggregateError([backupError, error], 'Cannot save composer backup or snapshot')
      throw error
    }
    if (backupError) throw backupError
  })
  writes.set(scope, write)
  const cleanup = () => { if (writes.get(scope) === write) writes.delete(scope) }
  void write.then(cleanup, cleanup)
  return write
}
