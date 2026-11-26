import { useCallback, useEffect } from 'react'
import { AlertCircle, CheckCircle2, Clock3 } from 'lucide-react'
import { ApiError } from '../api.ts'
import { useI18n } from '../i18n/provider.tsx'
import { stateLabel, type Locale } from '../i18n/index.ts'
import { cx, Badge } from '../components/ui.tsx'

/** Localize a failure while keeping the raw server message available. */
export function useDescribeError() {
  const { t } = useI18n()

  // Stable identity: pages use this inside `useCallback` dependencies, and a
  // new function per render would re-run their loaders on every render.
  return useCallback(
    (reason: unknown): string => {
      if (reason instanceof ApiError) {
        if (reason.status === 401) {
          return t('auth.sessionExpired')
        }

        if (reason.status === 503) {
          return t('error.temporarilyUnavailable')
        }

        return reason.message
      }

      if (reason instanceof Error) {
        return reason.message
      }

      return t('error.unexpected')
    },
    [t],
  )
}

const OK_STATES = new Set([
  'created',
  'running',
  'idle',
  'succeeded',
  'ready',
  'available',
  'ok',
])

const BAD_STATES = new Set([
  'failed',
  'expired',
  'timed_out',
  'interrupted',
  'not_installed',
  'not_ready',
  'error',
])

export function stateTone(code: string): 'ok' | 'warn' | 'bad' | 'neutral' {
  if (OK_STATES.has(code)) {
    return 'ok'
  }

  if (BAD_STATES.has(code)) {
    return 'bad'
  }

  if (code === 'unknown') {
    return 'neutral'
  }

  return 'warn'
}

export function StatusBadge({
  code,
  table,
  locale,
}: {
  code: string
  table: Record<string, Record<Locale, string>>
  locale: Locale
}) {
  return <Badge tone={stateTone(code)}>{stateLabel(table, code, locale)}</Badge>
}

export function PageHeader({
  title,
  subtitle,
  actions,
  status,
}: {
  title: string
  subtitle: string
  actions?: React.ReactNode
  status?: React.ReactNode
}) {
  return (
    <header className="page-header">
      <div>
        <h1>{title}</h1>
        <p>{subtitle}</p>
      </div>
      <div className="page-header-actions">
        {status}
        {actions}
      </div>
    </header>
  )
}

export function MetricCard({
  icon,
  label,
  value,
  note,
}: {
  icon: React.ReactNode
  label: string
  value: string
  note: string
}) {
  return (
    <article className="ui-card metric-card">
      <div className="metric-card-head">
        <span>{label}</span>
        <span className="metric-icon" aria-hidden="true">
          {icon}
        </span>
      </div>
      <strong>{value}</strong>
      <small>{note}</small>
    </article>
  )
}

export function ErrorNotice({
  message,
  onRetry,
  title,
}: {
  message: string
  onRetry?: () => void
  title?: string
}) {
  const { t } = useI18n()

  return (
    <div className="ui-alert ui-alert-danger" role="alert">
      <span className="ui-alert-icon">
        <AlertCircle size={18} aria-hidden="true" />
      </span>
      <div className="ui-alert-body">
        <strong>{title ?? t('error.operationFailed')}</strong>
        <p className="ui-alert-text">{message}</p>
        {onRetry && (
          <button type="button" className="ui-button ui-secondary ui-compact" onClick={onRetry}>
            {t('common.retry')}
          </button>
        )}
      </div>
    </div>
  )
}

export function SuccessNote({ message }: { message: string }) {
  return (
    <div className="ui-alert ui-alert-success" role="status">
      <span className="ui-alert-icon">
        <CheckCircle2 size={18} aria-hidden="true" />
      </span>
      <div className="ui-alert-body">
        <p className="ui-alert-text">{message}</p>
      </div>
    </div>
  )
}

export function TimeAgo({ unix }: { unix: number | null | undefined }) {
  const { format } = useI18n()

  return (
    <span className="technical-time">
      <Clock3 size={13} aria-hidden="true" />
      {format.dateTime(unix)}
    </span>
  )
}

export function TechnicalId({ value, label }: { value: string; label: string }) {
  return (
    <code className={cx('technical-id')} title={value} aria-label={label}>
      {value}
    </code>
  )
}

/** Pages react to the palette's "refresh" action without remounting. */
export function useRefreshEvent(handler: () => void) {
  useEffect(() => {
    window.addEventListener('workspace:refresh', handler)

    return () => window.removeEventListener('workspace:refresh', handler)
  }, [handler])
}

export function dispatchRefresh() {
  window.dispatchEvent(new Event('workspace:refresh'))
}
