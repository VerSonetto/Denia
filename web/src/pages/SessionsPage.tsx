import { useEffect, useState } from 'react'
import * as api from '../api'
import { t } from '../i18n'
import type { Notify } from '../App'
import { BrandMark } from '../components/icons'
import { SessionView } from '../components/SessionView'
import type { ModelCatalog } from '../types'

export default function SessionsPage({
  activeId,
  notify,
  onRunningChange,
  onCreate,
}: {
  activeId: string | null
  notify: Notify
  onRunningChange: (id: string, running: boolean) => void
  onCreate: () => void
}) {
  const [catalog, setCatalog] = useState<ModelCatalog | null>(null)

  useEffect(() => {
    api
      .getCatalog()
      .then(setCatalog)
      .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
  }, [notify])

  if (!activeId) {
    return (
      <div className="hero">
        <BrandMark size={40} />
        <h1>{t('heroTitle')}</h1>
        <p>{t('heroSub')}</p>
        <button className="btn" onClick={onCreate}>
          + {t('newSession')}
        </button>
      </div>
    )
  }
  return (
    <SessionView
      id={activeId}
      catalog={catalog}
      notify={notify}
      onRunningChange={onRunningChange}
    />
  )
}
