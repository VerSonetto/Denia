import { t } from '../../i18n'
import { IconPrompt } from '../icons'
import { DisclosureRow } from './DisclosureRow'

export function SystemPromptRow({ text }: { text: string }) {
  return <DisclosureRow title={t('systemPromptTitle')} icon={<IconPrompt size={14} />} text={text} />
}
