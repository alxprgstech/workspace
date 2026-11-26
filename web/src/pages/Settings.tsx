import { useCallback, useEffect, useState } from 'react'
import { Monitor, Moon, RefreshCw, Sun } from 'lucide-react'
import { loadCapabilities, loadRuntime, type CapabilityCatalog, type RuntimeStatus } from '../api.ts'
import { useI18n } from '../i18n/provider.tsx'
import { SERVICE_STATES, type MessageKey } from '../i18n/index.ts'
import {
  Badge,
  Button,
  Card,
  Select,
  Skeleton,
  Table,
} from '../components/ui.tsx'
import type { ThemeController } from '../theme/useTheme.ts'
import { ErrorNotice, PageHeader, StatusBadge, useDescribeError, useRefreshEvent } from './shared.tsx'

const THEMES = [
  { value: 'dark', key: 'settings.themeDark', icon: Moon },
  { value: 'light', key: 'settings.themeLight', icon: Sun },
  { value: 'system', key: 'settings.themeSystem', icon: Monitor },
] as const

/** Known server-owned descriptions are localized; unknown ids keep the raw text. */
const SERVICE_KEYS: Record<string, MessageKey> = {
  http_api: 'settings.service.http_api',
  sso_web_session: 'settings.service.sso_web_session',
  mcp_gateway: 'settings.service.mcp_gateway',
  docker_runtime: 'settings.service.docker_runtime',
  workspace_manager: 'settings.service.workspace_manager',
  job_execution: 'settings.service.job_execution',
}

const MODULE_KEYS: Record<string, MessageKey> = {
  base: 'settings.module.base',
  git: 'settings.module.git',
  rust: 'settings.module.rust',
  cpp: 'settings.module.cpp',
  python: 'settings.module.python',
  ffmpeg: 'settings.module.ffmpeg',
  'whisper-vad': 'settings.module.whisper-vad',
}

function localizedDescription(
  t: (key: MessageKey) => string,
  keys: Record<string, MessageKey>,
  id: string,
  fallback: string,
): string {
  const key = keys[id]

  return key ? t(key) : fallback
}

export function SettingsPage({
  theme,
  active,
}: {
  theme: ThemeController
  active: boolean
}) {
  const { t, locale, setLocale } = useI18n()
  const describe = useDescribeError()

  const [runtime, setRuntime] = useState<RuntimeStatus | null>(null)
  const [catalog, setCatalog] = useState<CapabilityCatalog | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const reload = useCallback(async () => {
    setLoading(true)

    try {
      const [nextRuntime, nextCatalog] = await Promise.all([loadRuntime(), loadCapabilities()])

      setRuntime(nextRuntime)
      setCatalog(nextCatalog)
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

  const readiness = runtime?.readiness ?? 'unavailable'

  return (
    <div className="page-stack">
      <PageHeader
        title={t('settings.title')}
        subtitle={t('settings.subtitle')}
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
      />

      {error && <ErrorNotice message={error} onRetry={() => void reload()} />}

      <Card>
        <div className="section-title">
          <h2>{t('settings.appearanceTitle')}</h2>
          <p>{t('settings.appearanceLead')}</p>
        </div>

        <div className="settings-grid">
          <div className="ui-field">
            <span id="language-label">{t('settings.language')}</span>
            <Select
              aria-labelledby="language-label"
              value={locale}
              onChange={event => setLocale(event.target.value === 'ru' ? 'ru' : 'en')}
            >
              <option value="ru">{t('settings.languageRu')}</option>
              <option value="en">{t('settings.languageEn')}</option>
            </Select>
          </div>

          <fieldset className="theme-field">
            <legend>{t('settings.theme')}</legend>
            <div className="segmented">
              {THEMES.map(entry => {
                const Icon = entry.icon
                const selected = theme.preference === entry.value

                return (
                  <button
                    key={entry.value}
                    type="button"
                    className={selected ? 'segment is-selected' : 'segment'}
                    aria-pressed={selected}
                    onClick={() => theme.setPreference(entry.value)}
                  >
                    <Icon size={16} aria-hidden="true" />
                    {t(entry.key)}
                  </button>
                )
              })}
            </div>
          </fieldset>
        </div>
      </Card>

      <Card>
        <div className="section-title">
          <h2>{t('settings.runtimeTitle')}</h2>
        </div>

        {runtime ? (
          <div className="stack">
            <div className="row wrap">
              <Badge tone={readiness === 'ready' ? 'ok' : readiness === 'degraded' ? 'warn' : 'bad'}>
                {t(`dashboard.readiness.${readiness === 'ready' ? 'ready' : readiness === 'degraded' ? 'degraded' : readiness === 'unconfigured' ? 'unconfigured' : 'unavailable'}`)}
              </Badge>
              <Badge tone={runtime.infrastructure_ready ? 'ok' : 'bad'}>
                {t('dashboard.infrastructure')}:{' '}
                {runtime.infrastructure_ready ? t('dashboard.ready') : t('dashboard.notReady')}
              </Badge>
              <Badge tone={runtime.execution_ready ? 'ok' : 'bad'}>
                {t('dashboard.execution')}:{' '}
                {runtime.execution_ready ? t('dashboard.ready') : t('dashboard.notReady')}
              </Badge>
              <span className="muted small">
                {t('settings.backend')}: <code>{runtime.backend ?? '—'}</code>
              </span>
              <span className="muted small">
                {t('settings.schemaVersion')}: <code>{catalog?.schema_version ?? '—'}</code>
              </span>
            </div>

            {runtime.blocking.length > 0 && (
              <div className="ui-alert ui-alert-warning" role="alert">
                <div className="ui-alert-body">
                  <strong>{t('dashboard.blocking')}</strong>
                  <ul className="alert-list">
                    {runtime.blocking.map(reason => (
                      <li key={reason}>{reason}</li>
                    ))}
                  </ul>
                </div>
              </div>
            )}

            <details className="ui-details">
              <summary>
                <span>
                  {t('dashboard.runtimeChecks')} ({runtime.checks.length})
                </span>
                <span className="summary-hint">{t('common.showDetails')}</span>
              </summary>
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
            </details>
          </div>
        ) : (
          <Skeleton lines={4} />
        )}
      </Card>

      <Card>
        <div className="section-title">
          <h2>{t('settings.servicesTitle')}</h2>
        </div>
        {catalog ? (
          catalog.services.length === 0 ? (
            <p className="muted">{t('settings.noServices')}</p>
          ) : (
            <Table label={t('settings.servicesTitle')}>
              <thead>
                <tr>
                  <th>{t('common.name')}</th>
                  <th>{t('common.status')}</th>
                  <th>{t('common.description')}</th>
                </tr>
              </thead>
              <tbody>
                {catalog.services.map(service => (
                  <tr key={service.id}>
                    <td>
                      <code>{service.id}</code>
                    </td>
                    <td>
                      <StatusBadge code={service.status} table={SERVICE_STATES} locale={locale} />
                    </td>
                    <td className="muted small">
                      {localizedDescription(t, SERVICE_KEYS, service.id, service.description)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </Table>
          )
        ) : (
          <Skeleton lines={3} />
        )}
      </Card>

      <Card>
        <div className="section-title">
          <h2>{t('settings.modulesTitle')}</h2>
        </div>
        {catalog ? (
          catalog.modules.length === 0 ? (
            <p className="muted">{t('settings.noModules')}</p>
          ) : (
            <Table label={t('settings.modulesTitle')}>
              <thead>
                <tr>
                  <th>{t('common.name')}</th>
                  <th>{t('common.status')}</th>
                  <th>{t('common.version')}</th>
                  <th>{t('common.description')}</th>
                </tr>
              </thead>
              <tbody>
                {catalog.modules.map(module => (
                  <tr key={module.id}>
                    <td>
                      <code>{module.id}</code>
                      <div className="muted small">{module.tools.join(', ')}</div>
                    </td>
                    <td>
                      <StatusBadge code={module.status} table={SERVICE_STATES} locale={locale} />
                    </td>
                    <td>{module.version}</td>
                    <td className="muted small">
                      {localizedDescription(t, MODULE_KEYS, module.id, module.description)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </Table>
          )
        ) : (
          <Skeleton lines={3} />
        )}
      </Card>
    </div>
  )
}
