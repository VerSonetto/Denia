import { useRef, useState } from 'react';
import { optimizePromptText } from '../../promptOptimizer';
import { notify } from '../../appStore';
import { t } from '../../i18n';
import type { ModelSelection } from '../../types';

/** 提示词优化:当前模型 + 最近 5 轮上下文,支持一键撤销。 */
export function usePromptOptimize({
  activeId,
  selection,
  inert,
  running,
  sending,
  prompt,
  applyDraftRef,
}: {
  activeId: string | null
  selection: ModelSelection | null
  inert: boolean
  running: boolean
  sending: boolean
  prompt: string
  /** 编辑器草稿重建入口。applyDraft 由 useComposerSlash 产出,晚于本 hook,
   *  故走 ref 转发(页面每渲染重赋,见 runCompactRef 同款套路)。 */
  applyDraftRef: { current: (text: string) => void }
}) {
  const [optimizing, setOptimizing] = useState(false)
  const [optimizedPrompt, setOptimizedPrompt] = useState<string | null>(null)
  const originalPromptRef = useRef('')
  const optimizeAbortRef = useRef<AbortController | null>(null)
  const optimizingRef = useRef(false)

  const handleOptimize = async () => {
    if (!activeId || !selection || optimizing || optimizingRef.current || inert || running || sending) return
    const original = prompt
    if (!original.trim()) return
    const sessionId = activeId
    const controller = new AbortController()
    optimizeAbortRef.current?.abort()
    optimizeAbortRef.current = controller
    optimizingRef.current = true
    originalPromptRef.current = original
    setOptimizedPrompt(null)
    setOptimizing(true)
    try {
      const optimized = await optimizePromptText({
        sessionId,
        prompt: original,
        selection,
        signal: controller.signal,
      })
      if (activeId !== sessionId || optimizeAbortRef.current !== controller) return
      applyDraftRef.current(optimized)
      originalPromptRef.current = original
      setOptimizedPrompt(optimized)
      notify('ok', t('optimizeDone'))
    } catch (error) {
      if (controller.signal.aborted || optimizeAbortRef.current !== controller) return
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      if (optimizeAbortRef.current === controller) {
        optimizeAbortRef.current = null
        optimizingRef.current = false
        setOptimizing(false)
      }
    }
  }

  const handleUndoOptimize = () => {
    if (!optimizedPrompt) return
    applyDraftRef.current(originalPromptRef.current)
  }

  return {
    optimizing,
    setOptimizing,
    optimizingRef,
    optimizedPrompt,
    setOptimizedPrompt,
    originalPromptRef,
    optimizeAbortRef,
    handleOptimize,
    handleUndoOptimize,
  }
}
