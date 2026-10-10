/**
 * 自动保存状态条:常驻可见(不是 toast),让用户随时知道"刚才那下存了没有"。
 * 保存中 / 已保存 · 时刻 / 失败原因 + 重试。
 */

import { t } from '../../i18n'
import { formatClock, sanitizeErrorText } from './catalogUtils'
import type { ProviderAutosave } from './useProviderAutosave'
import styles from './AutosaveStatus.module.css'

export function AutosaveStatus({
  autosave,
  align = 'right',
}: {
  autosave: ProviderAutosave
  align?: 'left' | 'right'
}) {
  const { status, savedAt, error } = autosave
  const tone =
    status === 'error' ? styles.error : status === 'saving' ? styles.saving : styles.saved
  return (
    <div className={`${styles.root}${align === 'right' ? ` ${styles.right}` : ''}`}>
      {status === 'saving' && (
        <span className={`${styles.pill} ${tone}`} role="status">
          <span className={styles.dot} aria-hidden="true" />
          {t('llm.autosave.saving')}
        </span>
      )}
      {status === 'saved' && savedAt !== null && (
        <span className={`${styles.pill} ${tone}`} role="status">
          <span className={styles.dot} aria-hidden="true" />
          {t('llm.autosave.saved', { time: formatClock(savedAt) })}
        </span>
      )}
      {status === 'error' && (
        <span className={`${styles.pill} ${tone}`} role="alert">
          <span className={styles.dot} aria-hidden="true" />
          <span className={styles.errorText}>
            {t('llm.autosave.error', { reason: sanitizeErrorText(error ?? '—') })}
          </span>
          <button type="button" className={styles.retry} onClick={() => void autosave.retry()}>
            {t('retry')}
          </button>
        </span>
      )}
      {status === 'idle' && <span className={styles.hint}>{t('llm.autosave.hint')}</span>}
    </div>
  )
}
