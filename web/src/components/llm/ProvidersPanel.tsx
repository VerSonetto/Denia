/**
 * 供应商子页:顶部工具条(计数 + 添加) + 网关卡片列表 + 空态插画 + 删除确认。
 * 卡片是 memo 叶子;编辑弹窗由 LlmPanel 统一挂载,这里只发触发。
 * 一切落盘(含删除)都走 autosave 通道,本页不直接写设置。
 */

import { memo, useState } from 'react'
import { t } from '../../i18n'
import { protocolLabel } from '../../modelCatalog'
import { Badge, Button } from './atoms/form'
import { AutosaveStatus } from './AutosaveStatus'
import { ConfirmDialog } from '../ConfirmDialog'
import { IconPlus } from '../icons'
import type { ProviderRoute } from './types'
import type { ProviderAutosave } from './useProviderAutosave'
import type { Notify } from '../../App'
import type { CredentialInfo } from '../../types'
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

function AddButton({ onAdd }: { onAdd: () => void }) {
  return (
    <Button variant="primary" onClick={onAdd}>
      <IconPlus size={13} />
      {t('addProvider')}
    </Button>
  )
}

export function ProvidersPanel({
  routes,
  credentials,
  notify,
  autosave,
  onOpenProbe,
  onAdd,
  onEdit,
}: {
  routes: ProviderRoute[]
  credentials: Record<string, CredentialInfo>
  notify: Notify
  autosave: ProviderAutosave
  onOpenProbe: (providerId?: string) => void
  onAdd: () => void
  onEdit: (route: string) => void
}) {
  const [removing, setRemoving] = useState<ProviderRoute | null>(null)

  const confirmRemove = async () => {
    if (!removing) return
    const target = removing
    setRemoving(null)
    const ok = await autosave.write(target.route, null)
    if (ok) notify('ok', t('providerRemoved'))
  }

  return (
    <div className={styles.root}>
      {/* 添加入口固定在顶部:列表再长也不用滚到底去找按钮。 */}
      <div className={styles.toolbar}>
        <div className={styles.toolbarCopy}>
          <span className={styles.toolbarTitle}>{t('modelProvidersLabel')}</span>
          <span className={styles.toolbarCount}>
            {t('llm.provider.count', { n: routes.length })}
          </span>
        </div>
        <div className={styles.toolbarActions}>
          <AutosaveStatus autosave={autosave} />
          {routes.length > 0 && <AddButton onAdd={onAdd} />}
        </div>
      </div>

      {routes.length === 0 ? (
        <div className={styles.empty}>
          <EmptyIllustration />
          <h4 className={styles.emptyTitle}>{t('llm.provider.emptyTitle')}</h4>
          <p className={styles.emptyHint}>{t('llm.provider.emptyHint')}</p>
          <AddButton onAdd={onAdd} />
        </div>
      ) : (
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
