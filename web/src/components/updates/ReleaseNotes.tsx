import { useMemo } from 'react'
import { localeRevision, t } from '../../i18n'
import { MarkdownText, type MarkdownLabels } from '../../markdown/MarkdownText'
import type { UpdateRelease } from '../../updateApi'
import { formatUpdateBytes, formatUpdateDate, safeUpdateUrl, updateText } from './updateText'

export function ReleaseNotes({ release }: { release: UpdateRelease | null }) {
  const text = updateText()
  const revision = localeRevision()
  const labels = useMemo<MarkdownLabels>(() => ({
    code: { copyLabel: t('copy'), copiedLabel: t('copied') }, footnotes: t('footnotes'),
  }), [revision])
  const published = release?.publishedAt ? formatUpdateDate(release.publishedAt) : null
  const url = safeUpdateUrl(release?.htmlUrl)
  return (
    <section className="update-release-notes" aria-label={text.releaseNotes}>
      <div className="update-notes-topline">
        <h3>{text.releaseNotes}</h3>
        {release && <span className="update-release-tag">{release.tag || `v${release.version}`}</span>}
      </div>
      {release && (
        <>
          <h4 className="update-release-name">{release.name || `Denia ${release.version}`}</h4>
          <div className="update-release-meta">
            {published && <time dateTime={release.publishedAt ?? undefined}>{text.published} {published}</time>}
            {release.prerelease && <span className="update-prerelease">{text.prerelease}</span>}
          </div>
        </>
      )}
      {release?.body.trim() ? (
        <div className="markdown update-markdown"><MarkdownText text={release.body} labels={labels} /></div>
      ) : (
        <p className="update-notes-empty">{release ? text.noNotes : text.notesPending}</p>
      )}
      {release?.installer && (
        <p className="update-installer-info">
          <span>{text.installer}</span>
          <span>{release.installer.name} · {formatUpdateBytes(release.installer.size)}</span>
        </p>
      )}
      {url && <a className="update-text-link" href={url} target="_blank" rel="noopener noreferrer">{text.releasePage}<span aria-hidden="true"> ↗</span></a>}
    </section>
  )
}
