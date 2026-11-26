import { useCallback, useEffect, useRef, useState } from 'react'
import {
  AlertTriangle,
  DownloadCloud,
  History,
  RefreshCw,
  RotateCcw,
  ServerCog,
  ShieldCheck,
  Sparkles,
} from 'lucide-react'
import {
  ApiError,
  applyUpdate,
  checkForUpdates,
  loadUpdateStatus,
  rollbackUpdate,
  type UpdateJournal,
  type UpdateStatus,
} from '../api.ts'
import { useI18n } from '../i18n/provider.tsx'
import { UPDATE_STAGES, UPDATE_STATES, stateLabel } from '../i18n/index.ts'
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  DefinitionList,
  Skeleton,
  Table,
} from '../components/ui.tsx'
import { ErrorNotice, PageHeader, useDescribeError, useRefreshEvent } from './shared.tsx'
import { useToast } from '../components/ui.tsx'

const ACTIVE_STATUSES = new Set(['prepared', 'running'])
const POLL_MS = 2500

function journalIsActive(journal: UpdateJournal | null | undefined): boolean {
  return !!journal && ACTIVE_STATUSES.has(journal.status)
}

function statusTone(status: string): 'ok' | 'warn' | 'bad' | 'info' | 'neutral' {
  switch (status) {
    case 'succeeded':
      return 'ok'
    case 'failed':
    case 'rollback_failed':
      return 'bad'
    case 'rolled_back':
      return 'warn'
    case 'running':
    case 'prepared':
      return 'info'
    default:
      return 'neutral'
  }
}

export function UpdatesPage({ active, isAdmin }: { active: boolean; isAdmin: boolean }) {
  const { t, locale, format } = useI18n()
  const describe = useDescribeError()
  const { notify } = useToast()

  const [status, setStatus] = useState<UpdateStatus | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [denied, setDenied] = useState(false)
  const [checking, setChecking] = useState(false)
  const [busy, setBusy] = useState(false)
  const [reconnecting, setReconnecting] = useState(false)
  const [actionError, setActionError] = useState<string | null>(null)
  const [confirmUpdate, setConfirmUpdate] = useState(false)
  const [activeJobsConfirmed, setActiveJobsConfirmed] = useState(false)
  const [confirmRollback, setConfirmRollback] = useState(false)
  const [allowDataRestore, setAllowDataRestore] = useState(false)
  const [lastReconnectAt, setLastReconnectAt] = useState<number | null>(null)

  const statusRef = useRef<UpdateStatus | null>(null)
  statusRef.current = status

  const load = useCallback(
    async (silent = false) => {
      if (!silent) {
        setLoading(true)
      }

      try {
        const next = await loadUpdateStatus()
        setStatus(next)
        setError(null)
        setDenied(false)

        if (reconnecting) {
          notify(t('updates.journalReconnected'), 'success')
          setLastReconnectAt(Date.now())
        }

        setReconnecting(false)
      } catch (reason) {
        if (reason instanceof ApiError && reason.status === 403) {
          setDenied(true)
          setError(null)
        } else if (journalIsActive(statusRef.current?.journal) || reconnecting) {
          setReconnecting(true)
        } else {
          setError(describe(reason))
        }
      } finally {
        if (!silent) {
          setLoading(false)
        }
      }
    },
    [describe, notify, reconnecting, t],
  )

  useEffect(() => {
    void load()
    // The page is mounted lazily; the first load is enough here.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  useEffect(() => {
    if (active) {
      void load(true)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active])

  useRefreshEvent(() => void load())

  const journal = status?.journal ?? null
  const journalActive = journalIsActive(journal)

  useEffect(() => {
    if (!journalActive && !reconnecting) {
      return
    }

    let cancelled = false
    const timer = window.setInterval(() => {
      if (!cancelled) {
        void load(true)
      }
    }, POLL_MS)

    return () => {
      cancelled = true
      window.clearInterval(timer)
    }
  }, [journalActive, reconnecting, load])

  const runCheck = async () => {
    setChecking(true)
    setActionError(null)

    try {
      const report = await checkForUpdates()
      const next = await loadUpdateStatus()
      setStatus(next)
      notify(
        report.status === 'update_available'
          ? `${t('updates.updateAvailable')}: ${report.available?.version ?? ''}`
          : t('updates.checkCompleted'),
        report.status === 'error' ? 'danger' : 'success',
      )
    } catch (reason) {
      setActionError(describe(reason))
    } finally {
      setChecking(false)
    }
  }

  const startUpdate = async () => {
    setBusy(true)
    setActionError(null)

    try {
      const result = await applyUpdate({ activeJobsConfirmed })
      setConfirmUpdate(false)
      setStatus(current => (current ? { ...current, journal: result.journal, busy: true } : current))
      setReconnecting(true)
      notify(t('updates.updateStarted'), 'info')
    } catch (reason) {
      setActionError(describe(reason))
    } finally {
      setBusy(false)
    }
  }

  const startRollback = async () => {
    setBusy(true)
    setActionError(null)

    try {
      const result = await rollbackUpdate({ allowDatabaseRestore: allowDataRestore })
      setConfirmRollback(false)
      setStatus(current => (current ? { ...current, journal: result.journal, busy: true } : current))
      setReconnecting(true)
      notify(t('updates.rollbackStarted'), 'info')
    } catch (reason) {
      setActionError(describe(reason))
    } finally {
      setBusy(false)
    }
  }

  if (denied || !isAdmin) {
    return (
      <div className="page-stack">
        <PageHeader title={t('updates.title')} subtitle={t('updates.subtitle')} />
        <Card>
          <div className="ui-empty">
            <div className="ui-empty-icon">
              <ShieldCheck size={22} aria-hidden="true" />
            </div>
            <strong>{t('updates.adminOnlyTitle')}</strong>
            <p>{t('updates.adminOnlyBody')}</p>
          </div>
        </Card>
      </div>
    )
  }

  const available = status?.last_check?.available ?? null
  const checkStatus = status?.last_check?.status ?? null
  const canInstall =
    !!available &&
    !!status?.last_check?.update_available &&
    !busy &&
    !journalActive &&
    !reconnecting &&
    checkStatus !== 'error'
  const rollback = status?.rollback ?? null
  const activeJobs = status?.active_jobs ?? 0

  return (
    <div className="page-stack">
      <PageHeader
        title={t('updates.title')}
        subtitle={t('updates.subtitle')}
        status={
          status ? (
            <Badge
              tone={
                journalActive
                  ? 'info'
                  : status.last_check?.update_available
                    ? 'warn'
                    : 'ok'
              }
            >
              {journalActive
                ? t('updates.journalTitle')
                : status.last_check?.update_available
                  ? t('updates.updateAvailable')
                  : t('updates.upToDate')}
            </Badge>
          ) : undefined
        }
        actions={
          <>
            <Button
              compact
              icon={<RefreshCw size={16} aria-hidden="true" />}
              loading={loading}
              onClick={() => void load()}
            >
              {t('updates.refresh')}
            </Button>
            <Button
              compact
              variant="primary"
              icon={<Sparkles size={16} aria-hidden="true" />}
              loading={checking}
              disabled={busy || journalActive || reconnecting}
              onClick={() => void runCheck()}
            >
              {checking ? t('updates.checking') : t('updates.check')}
            </Button>
          </>
        }
      />

      {reconnecting && (
        <div className="ui-alert ui-alert-info" role="status">
          <span className="ui-alert-icon">
            <ServerCog size={18} className="spinning" aria-hidden="true" />
          </span>
          <div className="ui-alert-body">
            <strong>{t('updates.restarting')}</strong>
            <p className="ui-alert-text">{t('updates.restartingBody')}</p>
            <p className="ui-alert-text technical-detail">{t('updates.reconnectHint')}</p>
          </div>
        </div>
      )}

      {error && <ErrorNotice message={error} onRetry={() => void load()} />}
      {actionError && <ErrorNotice message={actionError} />}

      <Card>
        <div className="section-title">
          <h2>{t('updates.currentTitle')}</h2>
          <p>{t('updates.currentLead')}</p>
        </div>

        {status ? (
          <div className="stack">
            <div className="row wrap">
              <Badge tone={status.installed.managed ? 'ok' : 'warn'}>
                {status.installed.managed ? t('updates.managed') : t('updates.probed')}
              </Badge>
              <Badge tone={activeJobs > 0 ? 'warn' : 'neutral'}>
                {t('updates.activeJobs', { count: activeJobs })}
              </Badge>
              {!status.update_supported && (
                <Badge tone="warn">{t('common.notAvailable')}</Badge>
              )}
            </div>

            <DefinitionList
              rows={[
                {
                  term: t('updates.installedVersion'),
                  value: <code>{status.installed.version}</code>,
                },
                {
                  term: t('updates.buildSha'),
                  value: <code className="technical-detail">{status.installed.git_sha}</code>,
                },
                {
                  term: t('updates.installedAt'),
                  value: format.dateTime(status.installed.installed_at_unix),
                },
                {
                  term: t('updates.schema'),
                  value: <code>{status.installed.schema_version}</code>,
                },
                {
                  term: t('updates.installDir'),
                  value: <code className="technical-detail">{status.installed.install_dir}</code>,
                },
              ]}
            />
          </div>
        ) : (
          <Skeleton lines={4} />
        )}
      </Card>

      <Card>
        <div className="section-title">
          <h2>{t('updates.availableTitle')}</h2>
          <p>{t('updates.availableLead')}</p>
        </div>

        {!status ? (
          <Skeleton lines={3} />
        ) : !status.last_check ? (
          <p className="muted">{t('updates.neverChecked')}</p>
        ) : (
          <div className="stack">
            <div className="row wrap">
              <Badge
                tone={
                  checkStatus === 'update_available'
                    ? 'info'
                    : checkStatus === 'up_to_date'
                      ? 'ok'
                      : checkStatus === 'error'
                        ? 'bad'
                        : 'warn'
                }
              >
                {checkStatus === 'update_available'
                  ? t('updates.updateAvailable')
                  : checkStatus === 'up_to_date'
                    ? t('updates.upToDate')
                    : checkStatus === 'error'
                      ? t('updates.checkError')
                      : t('updates.incompatible')}
              </Badge>
              <span className="muted small">
                {t('updates.checkedAt')}: {format.dateTime(status.last_check.checked_at_unix)}
              </span>
            </div>

            {status.last_check.message && (
              <p className="muted small">{status.last_check.message}</p>
            )}

            {available ? (
              <>
                <DefinitionList
                  rows={[
                    { term: t('common.version'), value: <code>{available.version}</code> },
                    {
                      term: t('updates.buildSha'),
                      value: <code className="technical-detail">{available.git_sha}</code>,
                    },
                    { term: t('updates.channel'), value: <code>{available.channel}</code> },
                    {
                      term: t('updates.compatibility'),
                      value: (
                        <div className="row wrap">
                          <Badge tone="neutral">
                            {t('updates.compatibilitySchema', {
                              min: available.compatibility.min_database_schema,
                              max: available.compatibility.max_database_schema,
                            })}
                          </Badge>
                          <Badge tone="neutral">
                            {t('updates.compatibilityRuntime', {
                              runtime: available.compatibility.runtime.join(' / '),
                            })}
                          </Badge>
                        </div>
                      ),
                    },
                    {
                      term: t('updates.source'),
                      value: <code className="technical-detail">{available.source}</code>,
                    },
                  ]}
                />

                {available.changelog.length > 0 && (
                  <div>
                    <h3 className="subsection-title">{t('updates.changelog')}</h3>
                    <ul className="update-changelog">
                      {available.changelog.map(line => (
                        <li key={line}>{line}</li>
                      ))}
                    </ul>
                  </div>
                )}

                <div className="row wrap">
                  <Button
                    variant="primary"
                    icon={<DownloadCloud size={16} aria-hidden="true" />}
                    disabled={!canInstall}
                    onClick={() => {
                      setActiveJobsConfirmed(false)
                      setActionError(null)
                      setConfirmUpdate(true)
                    }}
                  >
                    {t('updates.install')}
                  </Button>
                  {!status.last_check.update_available && checkStatus === 'up_to_date' && (
                    <span className="muted small">{t('updates.noUpdateBody')}</span>
                  )}
                  {journalActive && (
                    <span className="muted small">{t('updates.busy')}</span>
                  )}
                </div>
              </>
            ) : (
              <p className="muted">{t('updates.noRelease')}</p>
            )}
          </div>
        )}
      </Card>

      {journal && (
        <Card>
          <div className="section-title">
            <h2>{t('updates.journalTitle')}</h2>
            <p>{t('updates.progressLead')}</p>
          </div>

          <div className="stack">
            <div className="row wrap">
              <Badge tone={statusTone(journal.status)}>
                {stateLabel(UPDATE_STATES, journal.status, locale)}
              </Badge>
              <Badge tone="neutral">
                {journal.action === 'rollback' ? t('updates.rollbackTitle') : t('updates.title')}
              </Badge>
              <span className="muted small">
                {stateLabel(UPDATE_STAGES, journal.stage, locale)} · {journal.progress}%
              </span>
              <span className="muted small">
                {t('updates.startedAt')}: {format.dateTime(journal.started_at_unix)}
              </span>
            </div>

            <div
              className="ui-progress"
              role="progressbar"
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={journal.progress}
              aria-label={t('updates.progress')}
            >
              <span style={{ width: `${Math.max(2, Math.min(100, journal.progress))}%` }} />
            </div>

            <p className="muted small">
              {journal.from_version} → {journal.to_version}
              {journal.message ? ` · ${journal.message}` : ''}
            </p>

            {journal.error && (
              <div className="ui-alert ui-alert-danger" role="alert">
                <span className="ui-alert-icon">
                  <AlertTriangle size={18} aria-hidden="true" />
                </span>
                <div className="ui-alert-body">
                  <strong>{t('updates.errorTitle')}</strong>
                  <p className="ui-alert-text technical-detail">{journal.error}</p>
                </div>
              </div>
            )}

            {journal.warnings.length > 0 && (
              <div className="ui-alert ui-alert-warning" role="status">
                <div className="ui-alert-body">
                  <strong>{t('updates.warnings')}</strong>
                  <ul className="alert-list">
                    {journal.warnings.map(warning => (
                      <li key={warning}>{warning}</li>
                    ))}
                  </ul>
                </div>
              </div>
            )}

            <details className="ui-details">
              <summary>
                <span>
                  {t('updates.steps')} ({journal.steps.length})
                </span>
                <span className="summary-hint">{t('common.showDetails')}</span>
              </summary>
              {journal.steps.length === 0 ? (
                <p className="muted">{t('updates.noSteps')}</p>
              ) : (
                <Table label={t('updates.steps')}>
                  <thead>
                    <tr>
                      <th>{t('updates.stage')}</th>
                      <th>{t('common.status')}</th>
                      <th>{t('common.details')}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {journal.steps.map(step => (
                      <tr key={`${step.id}-${step.at_unix}`}>
                        <td>{stateLabel(UPDATE_STAGES, step.id, locale)}</td>
                        <td>
                          <Badge
                            tone={
                              step.status === 'completed'
                                ? 'ok'
                                : step.status === 'failed'
                                  ? 'bad'
                                  : step.status === 'running'
                                    ? 'info'
                                    : 'neutral'
                            }
                          >
                            {step.status}
                          </Badge>
                        </td>
                        <td className="muted small technical-detail">{step.detail ?? '—'}</td>
                      </tr>
                    ))}
                  </tbody>
                </Table>
              )}
            </details>

            <div className="row wrap">
              <span className="muted small technical-detail">
                {t('updates.logPath')}: <code>{journal.log_path}</code>
              </span>
              {lastReconnectAt && (
                <span className="muted small">
                  <History size={13} aria-hidden="true" /> {format.dateTime(lastReconnectAt / 1000)}
                </span>
              )}
            </div>
          </div>
        </Card>
      )}

      <Card>
        <div className="section-title">
          <h2>{t('updates.rollbackTitle')}</h2>
          <p>{t('updates.rollbackLead')}</p>
        </div>

        {rollback ? (
          <div className="stack">
            <DefinitionList
              rows={[
                {
                  term: t('updates.rollbackAvailable', {
                    version: rollback.from_version,
                    current: rollback.to_version,
                  }),
                  value: <code className="technical-detail">{rollback.backup_dir}</code>,
                },
                {
                  term: t('common.created'),
                  value: format.dateTime(rollback.created_at_unix),
                },
              ]}
            />

            {rollback.database_restore_requires_ack && (
              <div className="ui-alert ui-alert-warning" role="status">
                <div className="ui-alert-body">
                  <p className="ui-alert-text">{t('updates.rollbackDataWarning')}</p>
                </div>
              </div>
            )}

            <Button
              variant="danger"
              icon={<RotateCcw size={16} aria-hidden="true" />}
              disabled={busy || journalActive || reconnecting}
              onClick={() => {
                setAllowDataRestore(false)
                setActionError(null)
                setConfirmRollback(true)
              }}
            >
              {t('updates.rollback')}
            </Button>
          </div>
        ) : (
          <p className="muted">{t('updates.rollbackNoBackup')}</p>
        )}
      </Card>

      <ConfirmDialog
        open={confirmUpdate}
        title={t('updates.installTitle', { version: available?.version ?? '' })}
        body={
          <div className="stack">
            <p>{t('updates.installBody')}</p>
            {activeJobs > 0 && (
              <>
                <p className="ui-alert-text">
                  {t('updates.activeJobsWarning', { count: activeJobs })}
                </p>
                <label className="ui-checkbox">
                  <input
                    type="checkbox"
                    checked={activeJobsConfirmed}
                    onChange={event => setActiveJobsConfirmed(event.target.checked)}
                  />
                  {t('updates.activeJobsConfirm')}
                </label>
              </>
            )}
            {actionError && <p className="ui-alert-text technical-detail">{actionError}</p>}
          </div>
        }
        confirmLabel={t('updates.install')}
        cancelLabel={t('common.cancel')}
        busy={busy}
        onConfirm={() => {
          if (activeJobs > 0 && !activeJobsConfirmed) {
            setActionError(t('updates.confirmRequired'))
            return
          }

          void startUpdate()
        }}
        onCancel={() => {
          if (!busy) {
            setConfirmUpdate(false)
            setActionError(null)
          }
        }}
      />

      <ConfirmDialog
        open={confirmRollback}
        title={t('updates.rollbackConfirmTitle', { version: rollback?.from_version ?? '' })}
        body={
          <div className="stack">
            <p>
              {t('updates.rollbackConfirmBody', {
                current: rollback?.to_version ?? '',
                version: rollback?.from_version ?? '',
              })}
            </p>
            {rollback?.database_restore_requires_ack && (
              <>
                <p className="ui-alert-text">{t('updates.rollbackDataWarning')}</p>
                <label className="ui-checkbox">
                  <input
                    type="checkbox"
                    checked={allowDataRestore}
                    onChange={event => setAllowDataRestore(event.target.checked)}
                  />
                  {t('updates.rollbackAllowData')}
                </label>
              </>
            )}
            {actionError && <p className="ui-alert-text technical-detail">{actionError}</p>}
          </div>
        }
        confirmLabel={t('updates.rollback')}
        cancelLabel={t('common.cancel')}
        destructive
        busy={busy}
        onConfirm={() => {
          if (rollback?.database_restore_requires_ack && !allowDataRestore) {
            setActionError(t('updates.confirmRequired'))
            return
          }

          void startRollback()
        }}
        onCancel={() => {
          if (!busy) {
            setConfirmRollback(false)
            setActionError(null)
          }
        }}
      />
    </div>
  )
}
