import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Ban, Copy, RefreshCw, TerminalSquare } from 'lucide-react'
import { cancelJob, jobLogs, listJobs, type Job, type Workspace } from '../api.ts'
import { useI18n } from '../i18n/provider.tsx'
import { JOB_STATES } from '../i18n/index.ts'
import {
  Badge,
  Button,
  Card,
  CopyButton,
  EmptyState,
  Field,
  IconButton,
  Select,
  Table,
  useToast,
} from '../components/ui.tsx'
import {
  ErrorNotice,
  PageHeader,
  StatusBadge,
  TechnicalId,
  useDescribeError,
  useRefreshEvent,
} from './shared.tsx'

export function JobsPage({
  workspaces,
  active,
  routeWorkspaceId,
  onFilterChange,
}: {
  workspaces: Workspace[]
  active: boolean
  routeWorkspaceId?: string
  onFilterChange: (workspaceId?: string) => void
}) {
  const { t, locale, format } = useI18n()
  const describe = useDescribeError()
  const { notify } = useToast()

  const [workspaceId, setWorkspaceId] = useState(routeWorkspaceId ?? workspaces[0]?.id ?? '')
  const [jobs, setJobs] = useState<Job[]>([])
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [stream, setStream] = useState<'stdout' | 'stderr'>('stdout')
  const [logs, setLogs] = useState('')
  const [logsLoading, setLogsLoading] = useState(false)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const selectedRef = useRef<string | null>(null)

  selectedRef.current = selectedId

  useEffect(() => {
    if (routeWorkspaceId !== undefined && routeWorkspaceId !== workspaceId) {
      setWorkspaceId(routeWorkspaceId)
    }
  }, [routeWorkspaceId, workspaceId])

  useEffect(() => {
    if (!workspaceId && workspaces[0]) {
      setWorkspaceId(workspaces[0].id)
    }
  }, [workspaces, workspaceId])

  const reload = useCallback(async () => {
    setLoading(true)

    try {
      const next = await listJobs(workspaceId || undefined)
      setJobs(next)
      setError(null)

      if (selectedRef.current && !next.some(job => job.id === selectedRef.current)) {
        setSelectedId(null)
      }
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setLoading(false)
    }
  }, [describe, workspaceId])

  useEffect(() => {
    void reload()
  }, [reload])

  useRefreshEvent(() => void reload())

  const selected = useMemo(
    () => jobs.find(job => job.id === selectedId) ?? null,
    [jobs, selectedId],
  )

  const loadLogs = useCallback(
    async (job: Job, which: 'stdout' | 'stderr') => {
      setLogsLoading(true)

      try {
        const page = await jobLogs(job.id, which)
        setLogs(page.data || '')
      } catch (reason) {
        setError(describe(reason))
      } finally {
        setLogsLoading(false)
      }
    },
    [describe],
  )

  useEffect(() => {
    if (selected) {
      void loadLogs(selected, stream)
    } else {
      setLogs('')
    }
  }, [selectedId, stream]) // eslint-disable-line react-hooks/exhaustive-deps

  const hasActive = jobs.some(job => job.state === 'queued' || job.state === 'running')

  useEffect(() => {
    if (!active || !hasActive) {
      return
    }

    const timer = window.setInterval(() => void reload(), 3000)

    return () => window.clearInterval(timer)
  }, [active, hasActive, reload])

  useEffect(() => {
    if (!active || !selected || (selected.state !== 'running' && selected.state !== 'queued')) {
      return
    }

    const timer = window.setInterval(() => void loadLogs(selected, stream), 3000)

    return () => window.clearInterval(timer)
  }, [active, selected, stream, loadLogs])

  const copyLogs = async () => {
    try {
      await navigator.clipboard.writeText(logs)
      notify(t('common.copied'))
    } catch {
      notify(t('common.copyFailed'), 'danger')
    }
  }

  const cancel = async (job: Job) => {
    try {
      await cancelJob(job.id)
      await reload()

      if (selectedId === job.id) {
        void loadLogs(job, stream)
      }
    } catch (reason) {
      setError(describe(reason))
    }
  }

  return (
    <div className="page-stack">
      <PageHeader
        title={t('jobs.title')}
        subtitle={t('jobs.subtitle')}
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
        status={hasActive ? <Badge tone="warn">{t('jobs.autoRefresh')}</Badge> : undefined}
      />

      {error && <ErrorNotice message={error} onRetry={() => void reload()} />}

      <Card>
        <div className="toolbar">
          <Field label={t('jobs.filterWorkspace')} htmlFor="job-workspace">
            <Select
              id="job-workspace"
              value={workspaceId}
              onChange={event => {
                setWorkspaceId(event.target.value)
                setSelectedId(null)
                onFilterChange(event.target.value || undefined)
              }}
            >
              <option value="">{t('jobs.allWorkspaces')}</option>
              {workspaces.map(workspace => (
                <option key={workspace.id} value={workspace.id}>
                  {workspace.name}
                </option>
              ))}
            </Select>
          </Field>
        </div>

        {jobs.length === 0 ? (
          <EmptyState icon={<TerminalSquare size={22} />} title={t('jobs.empty')} />
        ) : (
          <Table label={t('jobs.title')}>
            <thead>
              <tr>
                <th>{t('jobs.colJob')}</th>
                <th>{t('jobs.colState')}</th>
                <th>{t('jobs.colCommand')}</th>
                <th>{t('jobs.colExit')}</th>
                <th>{t('jobs.colDuration')}</th>
                <th>{t('common.actions')}</th>
              </tr>
            </thead>
            <tbody>
              {jobs.map(job => {
                const duration =
                  job.started_at_unix !== null
                    ? Math.max(
                        0,
                        (job.finished_at_unix ?? Math.floor(Date.now() / 1000)) - job.started_at_unix,
                      )
                    : null

                return (
                  <tr key={job.id} className={selectedId === job.id ? 'is-selected' : undefined}>
                    <td>
                      <button
                        type="button"
                        className="link-button mono"
                        onClick={() => setSelectedId(job.id)}
                      >
                        {job.id.slice(0, 8)}
                      </button>
                      <div className="inline-copy muted">
                        <CopyButton value={job.id} />
                      </div>
                    </td>
                    <td>
                      <StatusBadge code={job.state} table={JOB_STATES} locale={locale} />
                    </td>
                    <td>
                      <code className="command-preview" title={job.argv.join(' ')}>
                        {job.argv.join(' ')}
                      </code>
                    </td>
                    <td>{job.exit_code ?? '—'}</td>
                    <td>{duration === null ? '—' : format.duration(duration)}</td>
                    <td>
                      {(job.state === 'queued' || job.state === 'running') && (
                        <IconButton
                          label={t('jobs.cancel')}
                          tone="danger"
                          onClick={() => void cancel(job)}
                        >
                          <Ban size={16} aria-hidden="true" />
                        </IconButton>
                      )}
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </Table>
        )}
      </Card>

      {selected && (
        <Card>
          <div className="logs-header">
            <div>
              <h2>{t('jobs.logsTitle')}</h2>
              <p className="muted small">
                <TechnicalId value={selected.id} label={t('common.id')} /> ·{' '}
                {selected.argv.join(' ')}
              </p>
            </div>
            <div className="tabs" role="tablist" aria-label={t('jobs.logsTitle')}>
              <button
                type="button"
                role="tab"
                aria-selected={stream === 'stdout'}
                className={stream === 'stdout' ? 'tab is-active' : 'tab'}
                onClick={() => setStream('stdout')}
              >
                {t('jobs.stdout')} ({format.bytes(selected.stdout_bytes)}
                {selected.stdout_truncated ? `, ${t('jobs.truncated')}` : ''})
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={stream === 'stderr'}
                className={stream === 'stderr' ? 'tab is-active' : 'tab'}
                onClick={() => setStream('stderr')}
              >
                {t('jobs.stderr')} ({format.bytes(selected.stderr_bytes)}
                {selected.stderr_truncated ? `, ${t('jobs.truncated')}` : ''})
              </button>
            </div>
          </div>

          {selected.error && <p className="muted small">{selected.error}</p>}

          <pre className="log-output" tabIndex={0} aria-label={t('jobs.logsTitle')}>
            {logs || t('jobs.logsEmpty')}
          </pre>

          <div className="row">
            <Button
              icon={<RefreshCw size={16} aria-hidden="true" />}
              loading={logsLoading}
              onClick={() => void loadLogs(selected, stream)}
            >
              {t('jobs.reloadLogs')}
            </Button>
            <Button icon={<Copy size={16} aria-hidden="true" />} onClick={() => void copyLogs()}>
              {t('jobs.copyLogs')}
            </Button>
          </div>
        </Card>
      )}

      {!selected && jobs.length > 0 && <p className="muted">{t('jobs.selectHint')}</p>}
    </div>
  )
}
