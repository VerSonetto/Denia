import { useEffect, useRef, useState } from 'react'
import { t } from '../../i18n'
import { useFollow } from '../../streaming/useFollow'
import { firstLine } from '../../toolDisplay'
import { IconChevron, IconThink } from '../icons'

export function ThinkRow({
  text,
  streaming = false,
}: {
  text: string
  /** 思考内容正在流式输出:强制展开;输出完毕自动折叠(手动点击随时可开合)。 */
  streaming?: boolean
}) {
  const [open, setOpen] = useState(streaming)
  // 思考框是 240px 的内部滚动框(外层对话容器不被撑高),跟随发生在它自己
  // 身上:流式期间框内吸底,框内手动上翻即脱跟,滚回底部自动恢复。
  // 引擎与对话流同一套(见 streaming/useFollow),不再各自为政。
  // alignOnAttach 只在流式时对齐:流式展开=看最新,流式结束后手动展开=从头读。
  const preRef = useRef<HTMLPreElement | null>(null)
  const follow = useFollow(preRef, {
    active: streaming,
    alignOnAttach: streaming,
  })
  // 思考摘要与正文按参考方案随流直接更新，不再使用旧的逐字揭示引擎。
  const revealing = streaming
  // 排空期间保持展开:残余还在长,这时折叠等于把没播完的字吞掉。
  useEffect(() => {
    setOpen(revealing)
  }, [revealing])
  const summary = firstLine(text, 80)
  return (
    <div className={`disc-row${open ? ' open' : ''}`}>
      <button
        className="disc-head"
        onClick={() => setOpen(!open)}
      >
        <span className="glyph">
          <IconThink size={16} />
        </span>
        <span className="title">{t('thinkTitle')}</span>
        {!open && (
          <>
            <span className="sep" />
            <span className="summary" style={{ fontFamily: 'inherit' }}>
              {summary}
            </span>
          </>
        )}
        <span className="chev">
          <IconChevron size={12} />
        </span>
      </button>
      {open && (
        <div className="disc-body">
          {/* 思考正文不套代码卡外框；限制仍放在自身滚动区，外层对话高度不被撑开。 */}
          <pre
            ref={follow.setPort}
            className={`think-content${streaming ? ' think-streaming' : ''}`}
          >
            {text}
          </pre>
        </div>
      )}
    </div>
  )
}
