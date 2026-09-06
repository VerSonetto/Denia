/**
 * 设置弹窗「模型」Tab 的总面板:左侧子导航 + 右侧详情两栏;
 * 窗口 <1024px 折叠为单列(导航变横向分段)。
 * 数据(设置/凭据/目录)在此加载,供应商/目录/连通性三个子页共享。
 */

import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { asRecord, namespaceOf } from './providerSettings'
import { ProvidersPanel } from './ProvidersPanel'
import { ModelsPanel } from './ModelsPanel'
import { ChatProbe } from './ChatProbe'
import { ProviderDrawer } from './ProviderDrawer'
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

const OPENAI_NS = 'llm-openai'

type SubPage = 'providers' | 'catalog' | 'probe'

export function LlmPanel({ notify }: { notify: Notify }) {
  const [settings, setSettings] = useState<SettingsDescribe | null>(null)
  const [credentials, setCredentials] = useState<Record<string, CredentialInfo>>({})
  const [catalog, setCatalog] = useState<ModelCatalog | null>(null)
  const [catalogError, setCatalogError] = useState<string | null>(null)
  const [sub, setSub] = useState<SubPage>('providers')
  const [drawer, setDrawer] = useState<{ route: string | null } | null>(null)
  const [probeSeed, setProbeSeed] = useState<string | undefined>(undefined)

  const settingsView = settings ? namespaceOf(settings, OPENAI_NS) : undefined

  const routes = useMemo<ProviderRoute[]>(() => {
    const providers = asRecord(settingsView?.value?.providers)
    return Object.entries(providers).map(([route, raw]) => ({
      route,
      profile: asRecord(raw) as unknown as OpenAiProfile,
    }))
  }, [settingsView])

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

  useEffect(() => {
    void load()
    void loadCatalog()
  }, [load, loadCatalog])

  // 设置被其他入口改动时(SSE 通知层已全局重拉设置),目录保持新鲜。
  const reloadAll = useCallback(() => {
    void load()
    void loadCatalog()
  }, [load, loadCatalog])

  const openProbe = useCallback((providerId?: string) => {
    setProbeSeed(providerId)
    setSub('probe')
  }, [])

  const editProvider = useCallback((route: string) => setDrawer({ route }), [])
  const addProvider = useCallback(() => setDrawer({ route: null }), [])

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
            settingsView={settingsView}
            credentials={credentials}
            notify={notify}
            onChanged={reloadAll}
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

      {drawer && settings && (
        <ProviderDrawer
          route={drawer.route}
          initial={
            drawer.route
              ? (routes.find((entry) => entry.route === drawer.route)?.profile ?? null)
              : null
          }
          revision={settingsView?.revision ?? 0}
          providers={asRecord(settingsView?.value?.providers)}
          credentials={credentials}
          notify={notify}
          onClose={() => setDrawer(null)}
          onSaved={reloadAll}
        />
      )}
    </div>
  )
}
