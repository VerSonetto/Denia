import { useCallback, useEffect, useRef, useState, type Dispatch, type SetStateAction } from 'react';
import * as api from '../../api';
import { saveComposer, type ComposerSnapshot } from '../../composerMemory';
import { notify } from '../../appStore';
import { t } from '../../i18n';
import { queuedPayload, pendingMessageKey, type OutgoingPayload } from './sessionState';
import type { QueuedMessage, PendingAttachment, UserMessageImage } from '../../types';
import type { TrajectoryQuote } from '../../trajectory';

export type PostMessageFn = (
  text: string,
  options?: { clearInput?: boolean; payload?: OutgoingPayload },
) => Promise<boolean>

type PastedImage = { name: string; mime: string; data: string; bytes: number; preview: string }

/**
 * 消息队列:AI 运行中输入的新消息,等本轮结束后自动发送。
 * 含轮次结束的自动发送 effect 与「立即发送/编辑/删除」三个队列操作。
 */
export function useMessageQueue({
  activeId,
  running,
  composerReadyRef,
  composerScope,
  postMessageRef,
  composerSnapshotRef,
  runningRef,
  mountedRef,
  applyDraft,
  promptRef,
  setPastedImages,
  setAttachments,
  setTrajQuotes,
  pastedImages,
  attachments,
  trajQuotes,
  setPendingMessages,
}: {
  activeId: string | null
  running: boolean
  composerReadyRef: { current: boolean }
  composerScope: string | null
  /** 每渲染重赋的 postMessage 镜像(赋值方在页面),effect 读它拿最新闭包。 */
  postMessageRef: { current: PostMessageFn }
  composerSnapshotRef: { current: ComposerSnapshot }
  runningRef: { current: boolean }
  mountedRef: { current: boolean }
  applyDraft: (text: string) => void
  promptRef: { current: HTMLDivElement | null }
  setPastedImages: (value: PastedImage[]) => void
  setAttachments: (value: PendingAttachment[]) => void
  setTrajQuotes: (value: TrajectoryQuote[]) => void
  pastedImages: PastedImage[]
  attachments: PendingAttachment[]
  trajQuotes: TrajectoryQuote[]
  setPendingMessages: Dispatch<SetStateAction<{ text: string; images?: UserMessageImage[] }[]>>
}) {
  const [queuedMessages, setQueuedMessages] = useState<QueuedMessage[]>([])
  const queuedMessagesRef = useRef(queuedMessages)
  queuedMessagesRef.current = queuedMessages
  const prevRunningRef = useRef(running)
  // 正在"立即发送"(sendNow)时抑制自动发送:sendNow 已移除目标消息,
  // 若此时 running 变 false,effect 不能再取队首发一次(会重复发送)。
  const suppressAutoFlushRef = useRef(false)

  /**
   * 轮次结束自动发送队首队列消息:
   * running 由 true → false 时,若队列非空,取出队首发送(不打断,因为已结束)。
   * 用 ref 保存最新 postMessage 与队列,避免 effect 闭包过期。
   */
  useEffect(() => {
    const prev = prevRunningRef.current
    prevRunningRef.current = running
    if (composerReadyRef.current && prev && !running && !suppressAutoFlushRef.current) {
      const next = queuedMessagesRef.current[0]
      if (next) {
        void postMessageRef
          .current(next.text, { clearInput: false, payload: queuedPayload(next) })
          .then((ok) => {
            if (ok) {
              setQueuedMessages((previous) => previous.filter((item) => item.id !== next.id))
              if (composerScope) void saveComposer(composerScope, { ...composerSnapshotRef.current,
                queue: composerSnapshotRef.current.queue.filter((item) => item.id !== next.id),
              }).catch((error) => notify('err', String(error)))
            }
          })
      }
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [running])

  /* ---- 队列操作:立即发送(打断)/ 编辑(回填输入框)/ 删除 ---- */

  /** 立即发送:先打断当前 AI,再直接发送这条队列消息。 */
  const sendNow = async (message: QueuedMessage) => {
    if (!activeId) return
    const sendQueuedMessage = postMessageRef.current
    suppressAutoFlushRef.current = true
    try {
      await api.cancelSession(activeId)
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
    // cancel 是异步的,等 driver 退出 running 再发送(running guard 会拒绝并发)。
    const deadline = Date.now() + 5000
    while (runningRef.current && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 150))
    }
    if (runningRef.current) {
      // 没能打断:消息保留在队列,提示用户。
      suppressAutoFlushRef.current = false
      notify('err', t('queueSendFailed'))
      return
    }
    if (!mountedRef.current) { suppressAutoFlushRef.current = false; return }
    // 收到服务端确认后才出队，切换或刷新不会丢失待发送消息。
    try {
      const ok = await sendQueuedMessage(message.text, {
        clearInput: false,
        payload: queuedPayload(message),
      })
      if (ok) {
        setQueuedMessages((previous) => previous.filter((item) => item.id !== message.id))
        if (composerScope) await saveComposer(composerScope, { ...composerSnapshotRef.current,
          queue: composerSnapshotRef.current.queue.filter((item) => item.id !== message.id),
        })
      }
    } finally {
      suppressAutoFlushRef.current = false
    }
  }

  /** 编辑:把消息(含图片/附件/引用)回填到输入框并移出队列(卡片随词典重建)。 */
  const editQueued = (message: QueuedMessage) => {
    applyDraft(message.text)
    // 回填是「整体覆盖」:队列消息就是当时输入区的全量快照,直接恢复。
    setPastedImages(message.images ?? [])
    setAttachments(message.attachments ?? [])
    setTrajQuotes((message.quotes ?? []) as TrajectoryQuote[])
    setQueuedMessages((previous) => previous.filter((item) => item.id !== message.id))
    promptRef.current?.focus()
  }

  /** 删除队列消息。 */
  const deleteQueued = (message: QueuedMessage) => {
    setQueuedMessages((previous) => previous.filter((item) => item.id !== message.id))
  }

  /**
   * 乐观行协调:
   * - push:发送 202 后注入乐观行。
   * - settle:流中已出现同文本 user-message(可能先于 push,
   *   因为 202 返回前服务端已落库并推送)→ 从列表移除并记录确认。
   * 确认记录带短 TTL(settle 先到时 push 不再注入):快照重放会把
   * 历史消息也 settle 一遍,永久计数会让之后"同文本重发"(回退后
   * 常见)的乐观行被误吞——TTL 过期即失效,只有紧邻 202 的竞态窗口
   * (2 秒)内才生效,这正是它要保护的范围。
   */
  const CONFIRM_TTL_MS = 2000
  const confirmedRef = useRef(new Map<string, number>())

  const settlePending = useCallback((message: { text: string; images?: UserMessageImage[] }) => {
    const key = pendingMessageKey(message)
    const now = Date.now()
    // 惰性清理:快照重放会写入大量历史 key,过期项在读路径顺手删。
    if (confirmedRef.current.size > 64) {
      for (const [k, expiresAt] of confirmedRef.current) {
        if (expiresAt <= now) confirmedRef.current.delete(k)
      }
    }
    confirmedRef.current.set(key, now + CONFIRM_TTL_MS)
    setPendingMessages((previous) => {
      const index = previous.findIndex((candidate) => pendingMessageKey(candidate) === key)
      if (index < 0) return previous
      const next = [...previous]
      next.splice(index, 1)
      return next
    })
  }, [])

  const pushPending = useCallback((text: string, images?: UserMessageImage[]) => {
    const message = { text, images }
    const key = pendingMessageKey(message)
    const expiresAt = confirmedRef.current.get(key)
    if (expiresAt !== undefined) {
      // 确认记录一次性消耗:202 返回前服务端已落库推送(同文本已出现在
      // 流中)→ 不再注入乐观行。TTL 之外(历史快照的 settle)不算数。
      confirmedRef.current.delete(key)
      if (Date.now() < expiresAt) return
    }
    setPendingMessages((previous) => [...previous, message])
  }, [])

  /**
   * 消息入队(AI 运行中):输入区的全部内容(文本 + 图片 + 附件 + 轨迹引用)
   * 整体搬进队列,输入区随之下空,本轮结束后自动发送。
   */
  const enqueueMessage = (text: string) => {
    setQueuedMessages((previous) => [
      ...previous,
      {
        id: `queue-${crypto.randomUUID()}`,
        text,
        images: pastedImages.length > 0 ? pastedImages : undefined,
        attachments: attachments.length > 0 ? attachments : undefined,
        quotes: trajQuotes.length > 0 ? trajQuotes : undefined,
      },
    ])
    applyDraft('')
    setPastedImages([])
    setAttachments([])
    setTrajQuotes([])
  }

  return {
    queuedMessages,
    setQueuedMessages,
    suppressAutoFlushRef,
    sendNow,
    editQueued,
    deleteQueued,
    confirmedRef,
    settlePending,
    pushPending,
    enqueueMessage,
  }
}

