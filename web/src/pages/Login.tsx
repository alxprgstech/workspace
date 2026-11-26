import { AlertCircle, Loader2, ShieldCheck } from 'lucide-react'
import { useI18n } from '../i18n/provider.tsx'
import type { MessageKey } from '../i18n/index.ts'
import { AppearanceControls } from '../components/Layout.tsx'
import { Brand } from '../components/Brand.tsx'
import { Button, Card } from '../components/ui.tsx'
import type { ThemeController } from '../theme/useTheme.ts'

export type AuthPhase = 'checking' | 'anonymous' | 'unavailable' | 'error' | 'authenticated'

const AUTH_ERROR_KEYS: Record<string, MessageKey> = {
  access_denied: 'auth.accessDenied',
  sso_unavailable: 'auth.ssoUnavailable',
  sso_denied: 'auth.ssoDenied',
  login_failed: 'auth.loginFailed',
  login_expired: 'auth.loginExpired',
  session_expired: 'auth.sessionExpired',
  storage_error: 'auth.storageError',
  invalid_request: 'auth.invalidRequest',
}

export function LoginPage({
  phase,
  authNotice,
  fault,
  theme,
  onRetry,
}: {
  phase: AuthPhase
  authNotice: string | null
  fault: string | null
  theme: ThemeController
  onRetry: () => void
}) {
  const { t } = useI18n()
  const noticeKey = authNotice
    ? (AUTH_ERROR_KEYS[authNotice] ?? 'auth.invalidRequest')
    : null

  return (
    <div className="auth-shell">
      <div className="auth-background" aria-hidden="true">
        <span />
        <span />
        <span />
      </div>

      <div className="auth-controls">
        <AppearanceControls theme={theme} compact />
      </div>

      <main className="auth-surface">
        <Brand variant="auth" />

        {phase === 'checking' && (
          <div className="auth-state" role="status">
            <Loader2 size={24} className="spinning" aria-hidden="true" />
            <h1>{t('auth.checkingTitle')}</h1>
            <p>{t('auth.checkingBody')}</p>
          </div>
        )}

        {phase === 'anonymous' && (
          <>
            <h1>{t('auth.signInTitle')}</h1>
            <p className="auth-lead">{t('auth.signInLead')}</p>

            {noticeKey && (
              <div className="ui-alert ui-alert-warning" role="alert">
                <span className="ui-alert-icon">
                  <AlertCircle size={18} aria-hidden="true" />
                </span>
                <div className="ui-alert-body">
                  <strong>{t('auth.failedTitle')}</strong>
                  <p className="ui-alert-text">{t(noticeKey)}</p>
                </div>
              </div>
            )}

            <a className="ui-button ui-primary auth-submit" href="/auth/login">
              <ShieldCheck size={18} aria-hidden="true" />
              {t('auth.signIn')}
            </a>

            <p className="auth-hint">{t('auth.signInHint')}</p>
            <p className="auth-footnote">
              <ShieldCheck size={14} aria-hidden="true" /> {t('auth.noTokens')}
            </p>
          </>
        )}

        {(phase === 'unavailable' || phase === 'error') && (
          <div className="auth-state" role="alert">
            <AlertCircle size={24} aria-hidden="true" />
            <h1>
              {phase === 'unavailable' ? t('auth.unavailableTitle') : t('auth.errorTitle')}
            </h1>
            <p>{phase === 'unavailable' ? t('auth.unavailableBody') : (fault ?? t('error.unexpected'))}</p>
            <Button variant="primary" onClick={onRetry}>
              {t('common.retry')}
            </Button>
          </div>
        )}
      </main>

      <Card className="auth-aside" as="aside">
        <h2>
          {t('app.product')} {t('app.name')}
        </h2>
        <p>{t('app.description')}</p>
        <ul className="auth-features">
          <li>{t('dashboard.execution')}</li>
          <li>{t('nav.workspaces')}</li>
          <li>{t('nav.artifacts')}</li>
        </ul>
      </Card>
    </div>
  )
}
