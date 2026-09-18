import { t } from './i18n'
import { isReasoningEffortId, type ReasoningEffortId } from './modelCatalog'

/** 思考强度档位的本地化显示名。 */
export function reasoningEffortLabel(id: string): string {
  if (!isReasoningEffortId(id)) return id
  switch (id as ReasoningEffortId) {
    case 'off':
      return t('effortOff')
    case 'low':
      return t('effortLow')
    case 'medium':
      return t('effortMedium')
    case 'high':
      return t('effortHigh')
    case 'xhigh':
      return t('effortXhigh')
    case 'max':
      return t('effortMax')
  }
}
