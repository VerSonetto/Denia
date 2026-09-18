import { useCallback, useEffect, useState } from 'react'
import * as api from '../api'
import { t } from '../i18n'
import { IconChevron, IconFolder } from './icons'

interface BrowseState {
  path: string
  parent: string | null
  dirs: string[]
}

function baseName(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, '')
  return trimmed.split(/[\\/]/).pop() || path
}

/**
 * Modal directory browser: drill into subdirectories, go up, and confirm the
 * current directory. A collapsed manual-path row covers paste-from-anywhere.
 */
export function DirPicker({
  onPick,
  onClose,
  onError,
}: {
  onPick: (path: string) => Promise<void>
  onClose: () => void
  onError: (message: string) => void
}) {
  const [state, setState] = useState<BrowseState | null>(null)
  const [loading, setLoading] = useState(false)
  const [manual, setManual] = useState(false)
  const [manualPath, setManualPath] = useState('')
  const [picking, setPicking] = useState(false)
  const [folderName, setFolderName] = useState<string | null>(null)

  const browse = useCallback(
    (path?: string) => {
      setLoading(true)
      api
        .browseDirs(path)
        .then(setState)
        .catch((error) => onError(error instanceof Error ? error.message : String(error)))
        .finally(() => setLoading(false))
    },
    [onError],
  )

  useEffect(() => {
    browse()
  }, [browse])

  const pick = async (path: string) => {
    setPicking(true)
    try {
      await onPick(path)
      onClose()
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error))
    } finally {
      setPicking(false)
    }
  }

  const createFolder = async () => {
    if (!state || !folderName?.trim()) return
    try {
      await api.mkdir(state.path, folderName.trim())
      setFolderName(null)
      browse(state.path)
    } catch (error) {
      onError(error instanceof Error ? error.message : String(error))
    }
  }

  return (
    <div className="picker-backdrop" onClick={onClose}>
      <div className="picker" onClick={(event) => event.stopPropagation()}>
        <div className="picker-head">
          <button
            className="picker-up"
            disabled={!state?.parent}
            onClick={() => state?.parent && browse(state.parent)}
            title={t('upDir')}
          >
            <span className="chev-up">
              <IconChevron size={12} />
            </span>
          </button>
          <span className="picker-cwd" title={state?.path}>
            {state ? baseName(state.path) : t('loading')}
          </span>
        </div>
        <div className="picker-list">
          {loading && <div className="picker-empty">{t('loading')}</div>}
          {!loading && state && state.dirs.length === 0 && (
            <div className="picker-empty">{t('noSubdirs')}</div>
          )}
          {!loading &&
            state?.dirs.map((name) => (
              <button
                key={name}
                className="picker-item"
                onClick={() =>
                  browse(
                    /^[A-Za-z]:\\$/.test(name)
                      ? name
                      : `${state.path.replace(/[\\/]+$/, '')}/${name}`,
                  )
                }
              >
                <IconFolder size={13} />
                <span>{name.replace(/\\+$/, '')}</span>
              </button>
            ))}
        </div>
        <div className="picker-foot">
          {folderName !== null ? (
            <div className="picker-manual">
              <input
                type="text"
                autoFocus
                placeholder={t('newFolder')}
                value={folderName}
                onChange={(event) => setFolderName(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === 'Enter') void createFolder()
                  if (event.key === 'Escape') setFolderName(null)
                }}
              />
              <button className="btn small" onClick={() => void createFolder()}>
                {t('save')}
              </button>
            </div>
          ) : (
            <button className="picker-manual-toggle" onClick={() => setFolderName('')}>
              + {t('newFolder')}
            </button>
          )}
          {manual ? (
            <div className="picker-manual">
              <input
                type="text"
                autoFocus
                placeholder={t('workspacePathPlaceholder')}
                value={manualPath}
                onChange={(event) => setManualPath(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === 'Enter' && manualPath.trim()) {
                    void pick(manualPath.trim())
                  }
                }}
              />
              <button
                className="btn small"
                disabled={!manualPath.trim() || picking}
                onClick={() => void pick(manualPath.trim())}
              >
                {t('save')}
              </button>
            </div>
          ) : (
            <button
              className="picker-manual-toggle"
              onClick={() => setManual(true)}
            >
              {t('manualPathToggle')}
            </button>
          )}
          <span className="spacer" />
          <button
            className="btn"
            disabled={!state || picking}
            onClick={() => state && void pick(state.path)}
          >
            {t('selectCurrentDir')}
          </button>
        </div>
      </div>
    </div>
  )
}
