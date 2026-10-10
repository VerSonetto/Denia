/**
 * 设置弹窗「模型」Tab 的总面板:左侧子导航 + 右侧详情两栏;
 * 窗口 <1024px 折叠为单列(导航变横向分段)。
 * 数据(设置/凭据/目录)在此加载,供应商/目录/连通性三个子页共享。
 * `llm-openai` 的唯一写入口(自动保存通道)也在这里,子页只发意图。
 */

import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { asRecord, namespaceOf, OPENAI_NS } from './providerSettings'
import { ProvidersPanel } from './ProvidersPanel'
import { ModelsPanel } from './ModelsPanel'
import { ChatProbe } from './ChatProbe'
import { ProviderModal } from './ProviderModal'
import { sortRoutesByRecent, useProviderAutosave } from './useProviderAutosave'
import { IconActivity, IconKey, IconStack } from '../icons'
import type { ProviderRoute } from './types'
import type { Notify } from '../../App'
import type {
  CredentialInfo,
  ModelCatalog,
  OpenAiProfile,
  SettingsDescribe,
} from '../../types'
import styles from './LlmPanel.module.css'

type SubPage = 'providers' | 'catalog' | 'probe'

export function LlmPanel({ notify }: { notify: Notify }) {
  const [settings, setSettings] = useState<SettingsDescribe | null>(null)
  const [credentials, setCredentials] = useState<Record<string, CredentialInfo>>({})
  const [catalog, setCatalog] = useState<ModelCatalog | null>(null)
  const [catalogError, setCatalogError] = useState<string | null>(null)
  const [sub, setSub] = useState<SubPage>('providers')
  const [modal, setModal] = useState<{ route: string | null } | null>(null)
  const [probeSeed, setProbeSeed] = useState<string | undefined>(undefined)

  const settingsView = settings ? namespaceOf(settings, OPENAI_NS) : undefined

  const load = useCallback(async () => {
    try {
      const next = await api.getSettings()
      setSettings(next)
      const view = next.namespaces.find((entry) => entry.ns === OPENAI_NS)
      const refs: string[] = []
      for (const profile of Object.values(asRecord(view?.value?.providers))) {
        const keyRef = asRecord(profile).apiKeyEnv
        if (typeof keyRef === 'string' && keyRef) refs.push(keyRef)
      }
      setCredentials(refs.length > 0 ? await api.describeCredentials(refs) : {})
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }, [notify])

  const loadCatalog = useCallback(async () => {
    setCatalogError(null)
    try {
      setCatalog(await api.getCatalog())
    } catch (error) {
      setCatalogError(error instanceof Error ? error.message : String(error))
    }
  }, [])

  // 落盘后把整块刷新一遍:凭据徽标、模型目录计数都跟着变。
  const reloadAll = useCallback(() => {
    void load()
    void loadCatalog()
  }, [load, loadCatalog])

  const autosave = useProviderAutosave({ notify, onChanged: reloadAll })
  const { prime } = autosave

  useEffect(() => {
    void load()
    void loadCatalog()
  }, [load, loadCatalog])

  /**
   * 每次拿到服务端快照,给自动保存通道打底(基线 + revision)。通道内部会避开
   * 在途写入,所以这里无条件调用。
   */
  useEffect(() => {
    if (!settings) return
    const view = namespaceOf(settings, OPENAI_NS)
    prime(asRecord(view?.value?.providers), view?.revision ?? 0)
  }, [settings, prime])

  const routes = useMemo<ProviderRoute[]>(() => {
    const providers = asRecord(settingsView?.value?.providers)
    const listed: ProviderRoute[] = Object.entries(providers).map(([route, raw]) => ({
      route,
      profile: asRecord(raw) as unknown as OpenAiProfile,
    }))
    // 刚新建/刚编辑过的网关置顶,方便「加完接着改」;其余保持原序。
    return sortRoutesByRecent(listed, autosave.recent)
  }, [settingsView, autosave.recent])

  const openProbe = useCallback((providerId?: string) => {
    setProbeSeed(providerId)
    setSub('probe')
  }, [])

  const editProvider = useCallback((route: string) => setModal({ route }), [])
  const addProvider = useCallback(() => setModal({ route: null }), [])

  const navItems: { id: SubPage; label: string; desc: string; icon: ReactNode; count?: number }[] = [
    {
      id: 'providers',
      label: t('llm.panel.providers'),
      desc: t('llm.panel.providersDesc'),
      icon: <IconKey size={15} />,
      count: routes.length,
    },
    {
      id: 'catalog',
      label: t('llm.panel.catalog'),
      desc: t('llm.panel.catalogDesc'),
      icon: <IconStack size={15} />,
      count: catalog?.groups.reduce((sum, group) => sum + group.models.length, 0),
    },
    {
      id: 'probe',
      label: t('llm.panel.probe'),
      desc: t('llm.panel.probeDesc'),
      icon: <IconActivity size={15} />,
    },
  ]

  return (
    <div className={styles.root}>
      <nav className={styles.nav} aria-label={t('settingsPaneModelsTitle')}>
        {navItems.map((item) => (
          <button
            key={item.id}
            type="button"
            className={`${styles.navItem}${sub === item.id ? ` ${styles.navActive}` : ''}`}
            aria-current={sub === item.id ? 'page' : undefined}
            onClick={() => setSub(item.id)}
          >
            <span className={styles.navIcon}>{item.icon}</span>
            <span className={styles.navCopy}>
              <span className={styles.navLabel}>{item.label}</span>
              <span className={styles.navDesc}>{item.desc}</span>
            </span>
            {item.count !== undefined && item.count > 0 && (
              <span className={styles.navCount}>{item.count}</span>
            )}
          </button>
        ))}
      </nav>

      <div className={styles.detail}>
        {sub === 'providers' && (
          <ProvidersPanel
            routes={routes}
            credentials={credentials}
            notify={notify}
            autosave={autosave}
            onOpenProbe={openProbe}
            onAdd={addProvider}
            onEdit={editProvider}
          />
        )}
        {sub === 'catalog' && (
          <ModelsPanel
            catalog={catalog}
            catalogError={catalogError}
            onReloadCatalog={() => void loadCatalog()}
            onEditProvider={editProvider}
            onAddProvider={addProvider}
          />
        )}
        {sub === 'probe' && (
          <ChatProbe routes={routes} catalog={catalog} initialProvider={probeSeed} />
        )}
      </div>

      {modal && settings && (
        <ProviderModal
          key={modal.route ?? '__new__'}
          route={modal.route}
          initial={
            modal.route
              ? (routes.find((entry) => entry.route === modal.route)?.profile ?? null)
              : null
          }
          credentials={credentials}
          notify={notify}
          autosave={autosave}
          onChanged={reloadAll}
          onClose={() => setModal(null)}
        />
      )}
    </div>
  )
}
