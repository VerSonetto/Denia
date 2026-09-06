/**
 * 供应商子页:网关卡片列表 + 空态插画 + 删除确认。
 * 卡片是 memo 叶子;编辑抽屉由 LlmPanel 统一挂载,这里只发触发。
 */

import { memo, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { protocolLabel } from '../../modelCatalog'
import { Badge, Button } from './atoms/form'
import { ConfirmDialog } from '../ConfirmDialog'
import type { ProviderRoute } from './types'
import type { Notify } from '../../App'
import type { CredentialInfo, NamespaceView } from '../../types'
import styles from './ProvidersPanel.module.css'

const ProviderCard = memo(function ProviderCard({
  route,
  profile,
  credential,
  onEdit,
  onRemove,
  onTest,
}: {
  route: string
  profile: ProviderRoute['profile']
  credential: CredentialInfo | undefined
  onEdit: () => void
  onRemove: () => void
  onTest: () => void
}) {
  const missing = !credential?.configured
  return (
    <article className={styles.card}>
      <div className={styles.cardHead}>
        <div className={styles.cardTitleBox}>
          <span className={styles.cardName}>{profile.displayName || route}</span>
          <span className={styles.cardRoute}>{route}</span>
        </div>
        {missing ? (
          <Badge tone="err">{t('apiKeyMissing')}</Badge>
        ) : (
          <Badge tone="ok">{t('apiKeySet')}</Badge>
        )}
      </div>
      <div className={styles.cardMeta}>
        <span className={styles.cardURL}>{profile.baseURL}</span>
        <span className={styles.cardProto}>{protocolLabel(profile.protocol)}</span>
        <span className={styles.cardCount}>
          {t('modelsLabel')}: {(profile.models ?? []).length || '—'}
        </span>
      </div>
      {missing && <p className={styles.cardWarn}>{t('llm.credential.missingHint')}</p>}
      <div className={styles.cardActions}>
        <Button small variant="ghost" onClick={onTest}>
          {t('llm.provider.testCta')}
        </Button>
        <Button small variant="ghost" onClick={onEdit}>
          {t('editProvider')}
        </Button>
        <Button small variant="danger" onClick={onRemove}>
          {t('removeProvider')}
        </Button>
      </div>
    </article>
  )
})

/** 极简线条插画:层叠网关 + 加号;纯 currentColor,无渐变。 */
function EmptyIllustration() {
  return (
    <svg
      className={styles.emptyArt}
      width="72"
      height="72"
      viewBox="0 0 72 72"
      fill="none"
      aria-hidden="true"
    >
      <rect x="12" y="14" width="48" height="14" rx="5" stroke="currentColor" strokeWidth="1.6" />
      <rect x="12" y="33" width="48" height="14" rx="5" stroke="currentColor" strokeWidth="1.6" opacity="0.55" />
      <rect x="12" y="52" width="48" height="8" rx="4" stroke="currentColor" strokeWidth="1.6" opacity="0.3" />
      <circle cx="22" cy="21" r="2.4" fill="currentColor" />
      <path d="M52 40h8M56 36v8" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
    </svg>
  )
}

export function ProvidersPanel({
  routes,
  settingsView,
  credentials,
  notify,
  onChanged,
  onOpenProbe,
  onAdd,
  onEdit,
}: {
  routes: ProviderRoute[]
  settingsView: NamespaceView | undefined
  credentials: Record<string, CredentialInfo>
  notify: Notify
  onChanged: () => void
  onOpenProbe: (providerId?: string) => void
  onAdd: () => void
  onEdit: (route: string) => void
}) {
  const [removing, setRemoving] = useState<ProviderRoute | null>(null)

  const confirmRemove = async () => {
    if (!removing || !settingsView) return
    const raw = (settingsView.value.providers as Record<string, unknown>) ?? {}
    const next: Record<string, unknown> = {}
    for (const [key, value] of Object.entries(raw)) {
      if (key !== removing.route) next[key] = value
    }
    try {
      await api.replaceNamespace('llm-openai', { providers: next }, settingsView.revision)
      notify('ok', t('providerRemoved'))
      onChanged()
    } catch (err) {
      notify('err', err instanceof Error ? err.message : String(err))
    } finally {
      setRemoving(null)
    }
  }

  return (
    <div className={styles.root}>
      {routes.length === 0 ? (
        <div className={styles.empty}>
          <EmptyIllustration />
          <h4 className={styles.emptyTitle}>{t('llm.provider.emptyTitle')}</h4>
          <p className={styles.emptyHint}>{t('llm.provider.emptyHint')}</p>
          <Button variant="primary" onClick={onAdd}>
            {t('addProvider')}
          </Button>
        </div>
      ) : (
        <>
          <div className={styles.list}>
            {routes.map(({ route, profile }) => (
              <ProviderCard
                key={route}
                route={route}
                profile={profile}
                credential={profile.apiKeyEnv ? credentials[profile.apiKeyEnv] : undefined}
                onEdit={() => onEdit(route)}
                onRemove={() => setRemoving({ route, profile })}
                onTest={() => onOpenProbe(route)}
              />
            ))}
          </div>
          <div>
            <Button variant="ghost" onClick={onAdd}>
              + {t('addProvider')}
            </Button>
          </div>
        </>
      )}

      <ConfirmDialog
        open={removing !== null}
        title={t('llm.provider.deleteTitle')}
        desc={t('llm.provider.deleteDesc', { name: removing?.profile.displayName || removing?.route || '' })}
        danger
        onConfirm={() => void confirmRemove()}
        onCancel={() => setRemoving(null)}
      />
    </div>
  )
}
