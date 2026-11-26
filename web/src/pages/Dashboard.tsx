import { Activity, Cpu, HardDrive, MemoryStick, RefreshCw, ShieldCheck } from 'lucide-react'
import type { DashboardData, RuntimeStatus } from '../api.ts'
import { useI18n } from '../i18n/provider.tsx'
import { plural } from '../i18n/index.ts'
import { Badge, Button, Card, CopyButton, DefinitionList, Skeleton, Table } from '../components/ui.tsx'
import {
  ErrorNotice,
  MetricCard,
  PageHeader,
  TechnicalId,
} from './shared.tsx'

const READINESS_KEYS = {
  ready: 'dashboard.readiness.ready',
  degraded: 'dashboard.readiness.degraded',
  unavailable: 'dashboard.readiness.unavailable',
  unconfigured: 'dashboard.readiness.unconfigured',
} as const

function readinessTone(readiness: string): 'ok' | 'warn' | 'bad' | 'neutral' {
  switch (readiness) {
    case 'ready':
      return 'ok'
    case 'degraded':
      return 'warn'
    case 'unavailable':
      return 'bad'
    default:
      return 'neutral'
  }
}

export function DashboardPage({
  data,
  runtime,
  loading,
  error,
  onRefresh,
}: {
  data: DashboardData | null
  runtime: RuntimeStatus | null
  loading: boolean
  error: string | null
  onRefresh: () => void
}) {
  const { t, locale, format } = useI18n()

  const readiness =
    runtime?.readiness ??
    (data?.readiness.runtime_ready ? 'ready' : runtime?.infrastructure_ready ? 'degraded' : 'unavailable')

  const blocking = runtime?.blocking ?? (data?.readiness.reason ? [data.readiness.reason] : [])
  const resources = data?.info.resources
  const cores = resources ? resources.cpu_millis / 1000 : 0
  const coresLabel = plural(locale, cores, {
    one: locale === 'ru' ? '{count} ядро' : '{count} core',
    few: locale === 'ru' ? '{count} ядра' : undefined,
    many: locale === 'ru' ? '{count} ядер' : undefined,
    other: locale === 'ru' ? '{count} ядра' : '{count} cores',
  }).replace('{count}', format.number(cores))

  return (
    <div className="page-stack">
      <PageHeader
        title={t('dashboard.title')}
        subtitle={t('dashboard.subtitle')}
        actions={
          <Button
            compact
            icon={<RefreshCw size={16} aria-hidden="true" />}
            loading={loading}
            onClick={onRefresh}
          >
            {t('common.refresh')}
          </Button>
        }
        status={
          data ? (
            <Badge tone={toneOfHealth(data.health.status)}>
              {t('dashboard.health')}: {data.health.status}
            </Badge>
          ) : undefined
        }
      />

      {error && <ErrorNotice message={error} onRetry={onRefresh} />}

      {!data && loading && (
        <Card>
          <Skeleton lines={6} />
        </Card>
      )}

      {data && (
        <>
          {blocking.length > 0 && (
            <div className="ui-alert ui-alert-warning" role="alert">
              <span className="ui-alert-icon">
                <ShieldCheck size={18} aria-hidden="true" />
              </span>
              <div className="ui-alert-body">
                <strong>{t('dashboard.blocking')}</strong>
                <ul className="alert-list">
                  {blocking.map(reason => (
                    <li key={reason}>{reason}</li>
                  ))}
                </ul>
              </div>
            </div>
          )}

          <div className="readiness-grid">
            <Card as="article" className="readiness-card">
              <span className="readiness-label">{t('dashboard.health')}</span>
              <Badge tone={toneOfHealth(data.health.status)}>{data.health.status}</Badge>
            </Card>
            <Card as="article" className="readiness-card">
              <span className="readiness-label">{t('dashboard.infrastructure')}</span>
              <Badge tone={runtime?.infrastructure_ready ? 'ok' : 'bad'}>
                {runtime?.infrastructure_ready ? t('dashboard.ready') : t('dashboard.notReady')}
              </Badge>
            </Card>
            <Card as="article" className="readiness-card">
              <span className="readiness-label">{t('dashboard.execution')}</span>
              <Badge tone={readinessTone(readiness)}>
                {t(READINESS_KEYS[readiness as keyof typeof READINESS_KEYS] ?? 'dashboard.readiness.unavailable')}
              </Badge>
            </Card>
          </div>

          {resources && (
            <section aria-labelledby="limits-title">
              <div className="section-title">
                <h2 id="limits-title">{t('common.configuredLimit')}</h2>
                <p>{t('dashboard.configuredLimitsNote')}</p>
              </div>
              <div className="metrics-grid">
                <MetricCard
                  icon={<Cpu size={18} />}
                  label={t('common.cpu')}
                  value={coresLabel}
                  note={t('common.configuredLimit')}
                />
                <MetricCard
                  icon={<MemoryStick size={18} />}
                  label={t('common.memory')}
                  value={format.bytes(resources.memory_mib * 1024 * 1024)}
                  note={t('common.configuredLimit')}
                />
                <MetricCard
                  icon={<HardDrive size={18} />}
                  label={t('common.disk')}
                  value={format.bytes(resources.storage_mib * 1024 * 1024)}
                  note={t('common.configuredLimit')}
                />
                <MetricCard
                  icon={<Activity size={18} />}
                  label={t('common.concurrency')}
                  value={format.number(resources.max_concurrent_jobs)}
                  note={t('common.configuredLimit')}
                />
              </div>
            </section>
          )}

          <Card>
            <h2>{t('dashboard.systemInfo')}</h2>
            <DefinitionList
              rows={[
                { term: t('dashboard.product'), value: `${data.info.product} ${data.info.version}` },
                { term: t('dashboard.apiVersion'), value: <code>{data.info.api_version}</code> },
                {
                  term: t('dashboard.platform'),
                  value: `${data.info.platform} / ${data.info.architecture}`,
                },
                {
                  term: t('dashboard.instanceId'),
                  value: (
                    <span className="inline-copy">
                      <TechnicalId value={data.info.storage.instance_id} label={t('dashboard.instanceId')} />
                      <CopyButton value={data.info.storage.instance_id} />
                    </span>
                  ),
                },
                { term: t('dashboard.sqlite'), value: <code>{data.info.storage.sqlite_version}</code> },
                { term: t('dashboard.schema'), value: format.number(data.info.storage.schema_version) },
                {
                  term: t('dashboard.journal'),
                  value: <code>{data.info.storage.journal_mode.toUpperCase()}</code>,
                },
                {
                  term: t('dashboard.instanceCreated'),
                  value: format.dateTime(data.info.storage.created_at_unix),
                },
                {
                  term: t('dashboard.maxWorkspaces'),
                  value: format.number(resources?.max_active_workspaces ?? 0),
                },
              ]}
            />
          </Card>

          <Card>
            <details className="ui-details">
              <summary>
                <span>
                  {t('dashboard.runtimeChecks')} ({runtime?.checks.length ?? 0})
                </span>
                <span className="summary-hint">{t('common.showDetails')}</span>
              </summary>
              {!runtime || runtime.checks.length === 0 ? (
                <p className="muted">{t('dashboard.noChecks')}</p>
              ) : (
                <Table label={t('dashboard.runtimeChecks')}>
                  <thead>
                    <tr>
                      <th>{t('dashboard.check')}</th>
                      <th>{t('dashboard.checkState')}</th>
                      <th>{t('dashboard.checkDetail')}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {runtime.checks.map(check => (
                      <tr key={check.id}>
                        <td>
                          <code>{check.id}</code>
                          <div className="muted small">
                            {check.required ? t('settings.required') : t('settings.optional')}
                          </div>
                        </td>
                        <td>
                          <Badge tone={check.ok ? 'ok' : check.required ? 'bad' : 'warn'}>
                            {check.ok
                              ? t('settings.checkOk')
                              : check.required
                                ? t('settings.checkFail')
                                : t('settings.checkInfo')}
                          </Badge>
                        </td>
                        <td className="technical-detail">{check.detail}</td>
                      </tr>
                    ))}
                  </tbody>
                </Table>
              )}
            </details>
          </Card>
        </>
      )}
    </div>
  )
}

function toneOfHealth(status: string): 'ok' | 'warn' | 'bad' {
  if (status === 'ok' || status === 'healthy') {
    return 'ok'
  }

  if (status === 'degraded') {
    return 'warn'
  }

  return 'bad'
}

