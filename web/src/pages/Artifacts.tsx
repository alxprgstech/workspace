import { useCallback, useEffect, useMemo, useState } from 'react'
import { Database, Download, RefreshCw, Trash2 } from 'lucide-react'
import {
  artifactLink,
  listArtifacts,
  revokeArtifact,
  type Artifact,
} from '../api.ts'
import { useI18n } from '../i18n/provider.tsx'
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  CopyButton,
  EmptyState,
  IconButton,
  Table,
  useToast,
} from '../components/ui.tsx'
import { ErrorNotice, PageHeader, useDescribeError, useRefreshEvent } from './shared.tsx'

function artifactState(artifact: Artifact, now: number): 'active' | 'revoked' | 'expired' {
  if (artifact.revoked) {
    return 'revoked'
  }

  if (artifact.expires_at_unix <= now) {
    return 'expired'
  }

  return 'active'
}

export function ArtifactsPage({ active }: { active: boolean }) {
  const { t, format } = useI18n()
  const describe = useDescribeError()
  const { notify } = useToast()

  const [artifacts, setArtifacts] = useState<Artifact[]>([])
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [pendingRevoke, setPendingRevoke] = useState<Artifact | null>(null)
  const [revoking, setRevoking] = useState(false)

  const reload = useCallback(async () => {
    setLoading(true)

    try {
      setArtifacts(await listArtifacts())
      setError(null)
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setLoading(false)
    }
  }, [describe])

  useEffect(() => {
    void reload()
  }, [reload])

  useEffect(() => {
    if (active) {
      void reload()
    }
  }, [active]) // eslint-disable-line react-hooks/exhaustive-deps

  useRefreshEvent(() => void reload())

  const link = async (artifact: Artifact) => {
    try {
      const download = await artifactLink(artifact.id)

      window.open(download.url, '_blank', 'noopener,noreferrer')
      notify(t('artifacts.linkCreated', { date: format.dateTime(download.expires_at_unix) }))
    } catch (reason) {
      setError(describe(reason))
    }
  }

  const confirmRevoke = async () => {
    if (!pendingRevoke) {
      return
    }

    setRevoking(true)

    try {
      await revokeArtifact(pendingRevoke.id)
      setPendingRevoke(null)
      await reload()
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setRevoking(false)
    }
  }

  const now = Math.floor(Date.now() / 1000)
  const counts = useMemo(() => {
    const result = { active: 0, revoked: 0, expired: 0 }

    for (const artifact of artifacts) {
      result[artifactState(artifact, now)] += 1
    }

    return result
  }, [artifacts, now])

  return (
    <div className="page-stack">
      <PageHeader
        title={t('artifacts.title')}
        subtitle={t('artifacts.subtitle')}
        actions={
          <Button
            compact
            icon={<RefreshCw size={16} aria-hidden="true" />}
            loading={loading}
            onClick={() => void reload()}
          >
            {t('common.refresh')}
          </Button>
        }
        status={
          <div className="row">
            <Badge tone="ok">
              {t('artifacts.active')}: {format.number(counts.active)}
            </Badge>
            <Badge tone="bad">
              {t('artifacts.revoked')}: {format.number(counts.revoked)}
            </Badge>
          </div>
        }
      />

      {error && <ErrorNotice message={error} onRetry={() => void reload()} />}

      <Card>
        {artifacts.length === 0 ? (
          <EmptyState icon={<Database size={22} />} title={t('artifacts.empty')} />
        ) : (
          <Table label={t('artifacts.title')}>
            <thead>
              <tr>
                <th>{t('artifacts.colName')}</th>
                <th>{t('artifacts.colSize')}</th>
                <th>{t('artifacts.colSha')}</th>
                <th>{t('artifacts.colExpires')}</th>
                <th>{t('artifacts.colState')}</th>
                <th>{t('common.actions')}</th>
              </tr>
            </thead>
            <tbody>
              {artifacts.map(artifact => {
                const state = artifactState(artifact, now)

                return (
                  <tr key={artifact.id}>
                    <td>
                      <span>{artifact.name}</span>
                      <div className="muted small">{artifact.media_type}</div>
                    </td>
                    <td>{format.bytes(artifact.size_bytes)}</td>
                    <td>
                      <span className="inline-copy">
                        <code className="sha" title={artifact.sha256}>
                          {artifact.sha256.slice(0, 16)}…
                        </code>
                        <CopyButton value={artifact.sha256} label={t('artifacts.copySha')} />
                      </span>
                    </td>
                    <td>{format.dateTime(artifact.expires_at_unix)}</td>
                    <td>
                      <Badge tone={state === 'active' ? 'ok' : state === 'revoked' ? 'bad' : 'warn'}>
                        {t(`artifacts.${state}` as 'artifacts.active')}
                      </Badge>
                    </td>
                    <td>
                      <div className="row-actions">
                        <Button
                          compact
                          icon={<Download size={15} aria-hidden="true" />}
                          disabled={state !== 'active'}
                          onClick={() => void link(artifact)}
                          title={t('artifacts.downloadHint')}
                        >
                          {t('common.download')}
                        </Button>
                        <IconButton
                          label={t('artifacts.revokeTitle')}
                          tone="danger"
                          disabled={state !== 'active'}
                          onClick={() => setPendingRevoke(artifact)}
                        >
                          <Trash2 size={16} aria-hidden="true" />
                        </IconButton>
                      </div>
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </Table>
        )}
      </Card>

      <ConfirmDialog
        open={pendingRevoke !== null}
        title={t('artifacts.revokeTitle')}
        body={t('artifacts.revokeBody', { name: pendingRevoke?.name ?? '' })}
        confirmLabel={t('artifacts.revokeConfirm')}
        destructive
        busy={revoking}
        onCancel={() => setPendingRevoke(null)}
        onConfirm={() => void confirmRevoke()}
      />
    </div>
  )
}
