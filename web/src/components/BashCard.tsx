import { memo } from 'react'
import { t } from '../i18n'
import type { BashOutput } from '../toolDisplay'

/**
 * bash 的输入/输出:用"引用"而不是"块"。
 *
 * 结构化数据(diff、清单)才配得上带底色的卡片;bash 是一段**流文本**,
 * 给它同款容器就是在跟它们抢同一个视觉层级 —— 而且黑白语义体系里再压一档
 * 底色只会显脏。所以这里用一条极淡的竖线把命令与输出"引"起来:与对话流
 * 的正文同底,靠缩进和竖线分组,形态上更接近 markdown 的引用块。
 *
 * 退出码非 0 时竖线整条转红;stderr 段落单列并着红 —— 这两处红是**信息**
 * (命令自己失败在哪),不是装饰,所以只在这里出现。
 */
export const BashCard = memo(function BashCard({
  command,
  output,
  exitCode,
}: {
  command: string
  output: BashOutput
  exitCode?: number
}) {
  const failed = exitCode !== undefined && exitCode !== 0
  const empty = output.stdout.length === 0 && !output.stderr
  return (
    <div className={`bash-quote${failed ? ' failed' : ''}`}>
      <div className="bash-line bash-cmd">
        <span className="bash-prompt" aria-hidden>
          $
        </span>
        <pre>{command}</pre>
      </div>
      {output.stdout.length > 0 && (
        <div className="bash-line">
          <pre>{output.stdout}</pre>
        </div>
      )}
      {output.stderr && (
        <div className="bash-line bash-stderr">
          <pre>{output.stderr}</pre>
        </div>
      )}
      {empty && <div className="bash-line bash-empty">{t('bashNoOutput')}</div>}
    </div>
  )
})
