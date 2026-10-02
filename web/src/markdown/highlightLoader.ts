type HighlightModule = typeof import('./highlight')

let implementation: HighlightModule | undefined
let pending: Promise<void> | undefined
let revision = 0
let unsubscribeGrammar: (() => void) | undefined
const listeners = new Set<() => void>()

function notify() {
  revision++
  for (const listener of listeners) listener()
}

function connectGrammar() {
  if (implementation && listeners.size > 0 && !unsubscribeGrammar) {
    unsubscribeGrammar = implementation.subscribeGrammarLoaded(notify)
  }
}

export function subscribeHighlightLoaded(listener: () => void): () => void {
  listeners.add(listener)
  connectGrammar()
  return () => {
    listeners.delete(listener)
    if (listeners.size === 0) {
      unsubscribeGrammar?.()
      unsubscribeGrammar = undefined
    }
  }
}

export function highlightRevision(): number {
  return revision
}

export function getHighlightModule(lang: string | undefined): HighlightModule | undefined {
  if (!lang?.trim()) return undefined
  if (implementation) return implementation
  pending ??= import('./highlight').then((module) => {
    implementation = module
    connectGrammar()
    notify()
  }).catch(() => {
    pending = undefined
  })
  return undefined
}
