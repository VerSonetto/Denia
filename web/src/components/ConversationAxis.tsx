import { useMemo } from 'react'
import type { TranscriptNode } from '../fold'
import { t } from '../i18n'

/**
 * 对话轴:聊天区左侧的竖向导航条,每个用户消息一个节点。
 * 悬浮显示消息预览,点击跳转并把对应消息居中到视口。
 * 节点纵向位置按消息在 transcript 中的相对顺序等比映射。
 */
export function ConversationAxis({
  nodes,
  scrollRef,
}: {
  nodes: TranscriptNode[]
  scrollRef: React.RefObject<HTMLDivElement | null>
}) {
  // 只给可见(未被概览折叠)的用户消息建轴点;anchor 是事件 seq,稳定。
  const markers = useMemo(() => {
    const users = nodes.filter(
      (n): n is Extract<TranscriptNode, { kind: 'user' }> =>
        n.kind === 'user' && n.anchor !== undefined,
    )
    const total = nodes.length
    return users.map((node) => {
      const index = nodes.indexOf(node)
      return {
        anchor: node.anchor!,
        text: node.text,
        // 相对位置 0..1;单条消息时居中(0.5)。
        ratio: total > 1 ? index / (total - 1) : 0.5,
      }
    })
  }, [nodes])

  if (markers.length === 0) return null

  const jump = (anchor: number) => {
    const scroller = scrollRef.current
    if (!scroller) return
    const target = scroller.querySelector<HTMLElement>(`[data-user-anchor="${anchor}"]`)
    if (!target) return
    // 垂直居中:block:'center' 让浏览器把目标对齐到滚动容器可视区中点。
    target.scrollIntoView({ behavior: 'smooth', block: 'center' })
  }

  return (
    <nav className="conversation-axis" aria-label={t('axisLabel')}>
      <div className="axis-track">
        {markers.map((m) => (
          <button
            key={m.anchor}
            type="button"
            className="axis-dot"
            style={{ top: `${m.ratio * 100}%` }}
            onClick={() => jump(m.anchor)}
            aria-label={m.text}
          >
            <span className="axis-tip" role="tooltip">
              {m.text}
            </span>
          </button>
        ))}
      </div>
    </nav>
  )
}
