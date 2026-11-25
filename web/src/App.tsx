import { useCallback, useEffect, useState } from 'react'
import {
  Activity,
  AlertCircle,
  Blocks,
  CheckCircle2,
  Cpu,
  Database,
  File as FileIcon,
  Folder,
  HardDrive,
  LayoutDashboard,
  Loader2,
  LogOut,
  MemoryStick,
  Moon,
  RefreshCw,
  Settings,
  ShieldAlert,
  Sun,
  Terminal,
  UserRound,
} from 'lucide-react'
import {
  ApiError,
  loadDashboard,
  loadSession,
  listWorkspaces,
  logout,
  type DashboardData,
  type SessionInfo,
  type Workspace,
} from './api'
import {
  ArtifactsSection,
  FilesSection,
  JobsSection,
  SettingsSection,
  WorkspacesSection,
} from './sections'

const sections = [
  { id: 'dashboard', name: 'Dashboard', icon: LayoutDashboard },
  { id: 'workspaces', name: 'Workspaces', icon: Folder },
  { id: 'jobs', name: 'Jobs', icon: Terminal },
  { id: 'files', name: 'Files', icon: FileIcon },
  { id: 'artifacts', name: 'Artifacts', icon: Database },
  { id: 'settings', name: 'Settings', icon: Settings },
]

type Phase = 'checking' | 'anonymous' | 'unavailable' | 'error' | 'authenticated'

const authErrors: Record<string, string> = {
  access_denied:
    'Your ALXPRGS SSO identity was verified, but access to this ' +
    'Workspace has not been approved. Contact the administrator.',
  sso_unavailable:
    'ALXPRGS SSO is temporarily unavailable. Try signing in again later.',
  sso_denied: 'The sign-in request was declined by ALXPRGS SSO.',
  login_failed: 'Signing in through ALXPRGS SSO failed. Try again.',
  login_expired: 'The sign-in attempt expired. Start a new sign-in.',
  session_expired: 'Your session expired. Sign in again.',
  storage_error:
    'Workspace could not complete the sign-in because of a local error.',
  invalid_request: 'The sign-in response was invalid. Start a new sign-in.',
}

function Brand() {
  return (
    <div className="brand">
      <div className="brand-icon">
        <Blocks size={25} />
      </div>
      <div>
        <strong>ALXPRGS</strong>
        <span>Workspace</span>
      </div>
    </div>
  )
}

interface ThemeButtonProps {
  dark: boolean
  onToggle: () => void
}

function ThemeButton({ dark, onToggle }: ThemeButtonProps) {
  return (
    <button
      className="icon-button"
      aria-label="Toggle color theme"
      onClick={onToggle}
    >
      {dark ? <Sun size={20} /> : <Moon size={20} />}
    </button>
  )
}

interface AuthPanelProps {
  dark: boolean
  onToggleTheme: () => void
  children: React.ReactNode
}

function AuthPanel({ dark, onToggleTheme, children }: AuthPanelProps) {
  return (
    <div className="auth-shell">
      <div className="auth-theme">
        <ThemeButton dark={dark} onToggle={onToggleTheme} />
      </div>

      <main className="auth-card">
        <Brand />
        {children}
      </main>
    </div>
  )
}

function App() {
  const [dark, setDark] = useState(() => {
    const stored = window.localStorage.getItem('workspace-theme')

    return stored
      ? stored === 'dark'
      : window.matchMedia('(prefers-color-scheme: dark)').matches
  })

  const [phase, setPhase] = useState<Phase>('checking')
  const [session, setSession] = useState<SessionInfo | null>(null)
  const [authNotice, setAuthNotice] = useState<string | null>(
    () => new URLSearchParams(window.location.search).get('auth_error'),
  )
  const [check, setCheck] = useState(0)
  const [fault, setFault] = useState<string | null>(null)
  const [loggingOut, setLoggingOut] = useState(false)
  const [signOutError, setSignOutError] = useState<string | null>(null)

  const [data, setData] = useState<DashboardData | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [revision, setRevision] = useState(0)
  const [section, setSection] = useState('dashboard')
  const [workspaces, setWorkspaces] = useState<Workspace[]>([])
  const [workspaceError, setWorkspaceError] = useState<string | null>(null)

  useEffect(() => {
    document.documentElement.dataset.theme = dark ? 'dark' : 'light'
    window.localStorage.setItem('workspace-theme', dark ? 'dark' : 'light')
  }, [dark])

  // Never keep OAuth callback parameters in the address bar.
  useEffect(() => {
    if (window.location.search || window.location.hash) {
      window.history.replaceState(null, '', window.location.pathname)
    }
  }, [])

  useEffect(() => {
    const controller = new AbortController()

    const run = async () => {
      setPhase('checking')
      setFault(null)

      for (let attempt = 0; attempt < 3; attempt += 1) {
        try {
          const info = await loadSession(controller.signal)

          if (controller.signal.aborted) {
            return
          }

          setSession(info)
          setPhase(info ? 'authenticated' : 'anonymous')

          return
        } catch (reason) {
          if (controller.signal.aborted) {
            return
          }

          // HTTP 503 means the session may still be refreshing.
          // Never sign the user out because of a transient outage.
          if (reason instanceof ApiError && reason.status === 503) {
            if (attempt < 2) {
              await new Promise(resolve => setTimeout(resolve, 1500 * (attempt + 1)))

              if (controller.signal.aborted) {
                return
              }

              continue
            }

            setPhase('unavailable')

            return
          }

          setFault(
            reason instanceof Error ? reason.message : String(reason),
          )
          setPhase('error')

          return
        }
      }
    }

    void run()

    return () => controller.abort()
  }, [check])

  useEffect(() => {
    if (phase !== 'authenticated') {
      return
    }

    const controller = new AbortController()

    setLoading(true)
    setError(null)

    loadDashboard(controller.signal)
      .then(result => {
        if (!controller.signal.aborted) {
          setData(result)
        }
      })
      .catch((reason: unknown) => {
        if (controller.signal.aborted) {
          return
        }

        if (reason instanceof ApiError && reason.status === 401) {
          setSession(null)
          setData(null)
          setAuthNotice('session_expired')
          setPhase('anonymous')

          return
        }

        setData(null)
        setError(
          reason instanceof ApiError && reason.status === 503
            ? 'Workspace is temporarily unavailable. Your session was ' +
              'kept; retry in a moment.'
            : reason instanceof Error
              ? reason.message
              : String(reason),
        )
      })
      .finally(() => {
        if (!controller.signal.aborted) {
          setLoading(false)
        }
      })

    return () => controller.abort()
  }, [phase, revision])

  const signOut = useCallback(async () => {
    setLoggingOut(true)
    setSignOutError(null)

    try {
      await logout()

      // Only a confirmed server-side sign-out ends the session locally.
      setSession(null)
      setData(null)
      setError(null)
      setAuthNotice(null)
      setPhase('anonymous')
    } catch (reason) {
      // The outcome is unknown: the server may still hold a live session.
      // Never present this as a successful sign-out.
      setSignOutError(
        reason instanceof ApiError && reason.status === 503
          ? 'Workspace could not confirm the sign-out because the server is ' +
            'temporarily unavailable. Your server session may still be active.'
          : reason instanceof Error
            ? `Workspace could not confirm the sign-out (${reason.message}). ` +
              'Your server session may still be active.'
            : 'Workspace could not confirm the sign-out. Your server session ' +
              'may still be active.',
      )
    } finally {
      setLoggingOut(false)
    }
  }, [])

  const refreshWorkspaces = useCallback(() => {
    listWorkspaces()
      .then(next => {
        setWorkspaces(next)
        setWorkspaceError(null)
      })
      .catch((reason: unknown) => {
        setWorkspaceError(
          reason instanceof Error ? reason.message : 'Cannot load workspaces',
        )
      })
  }, [])

  useEffect(() => {
    if (phase === 'authenticated') {
      refreshWorkspaces()
    }
  }, [phase, refreshWorkspaces])

  const themeButton = (
    <ThemeButton dark={dark} onToggle={() => setDark(value => !value)} />
  )

  if (phase !== 'authenticated' || !session) {
    return (
      <AuthPanel dark={dark} onToggleTheme={() => setDark(value => !value)}>
        {phase === 'checking' && (
          <div className="auth-state" role="status">
            <Loader2 size={26} className="spinning" />
            <h1>Checking session</h1>
            <p>Verifying your Workspace session.</p>
          </div>
        )}

        {phase === 'anonymous' && (
          <>
            <h1>Sign in to Workspace</h1>
            <p className="auth-lead">
              Use your ALXPRGS account. No tokens are stored in the browser.
            </p>

            {authNotice && (
              <div
                className={`notice ${
                  authNotice === 'access_denied' ? 'error' : 'warning'
                }`}
                role="alert"
              >
                <ShieldAlert size={20} />
                <div>
                  <strong>Sign-in failed</strong>
                  <p>{authErrors[authNotice] ?? authErrors.invalid_request}</p>
                </div>
              </div>
            )}

            <a className="button primary auth-button" href="/auth/login">
              <ShieldAlert size={19} />
              Sign in with ALXPRGS SSO
            </a>

            <p className="auth-hint">
              Sign-in happens on the ALXPRGS SSO server. Workspace keeps only
              an encrypted server-side session.
            </p>
          </>
        )}

        {phase === 'unavailable' && (
          <div className="auth-state" role="alert">
            <AlertCircle size={26} />
            <h1>Workspace is temporarily unavailable</h1>
            <p>
              Your session was not signed out. This usually means a token
              refresh or a server restart is in progress.
            </p>
            <button
              className="button primary"
              onClick={() => setCheck(value => value + 1)}
            >
              Try again
            </button>
          </div>
        )}

        {phase === 'error' && (
          <div className="auth-state" role="alert">
            <AlertCircle size={26} />
            <h1>Cannot check the session</h1>
            <p>{fault ?? 'Unexpected error'}</p>
            <button
              className="button primary"
              onClick={() => setCheck(value => value + 1)}
            >
              Try again
            </button>
          </div>
        )}
      </AuthPanel>
    )
  }

  const resources = data?.info.resources

  const metrics = resources ? [
    {
      label: 'CPU budget',
      value: `${resources.cpu_millis / 1000} cores`,
      icon: Cpu,
    },
    {
      label: 'Memory budget',
      value: `${resources.memory_mib / 1024} GiB`,
      icon: MemoryStick,
    },
    {
      label: 'Storage budget',
      value: `${resources.storage_mib / 1024} GiB`,
      icon: HardDrive,
    },
    {
      label: 'Concurrent jobs',
      value: String(resources.max_concurrent_jobs),
      icon: Activity,
    },
  ] : []

  const displayName = session.user.username ?? session.user.subject

  const roleLabel = session.user.role === 'admin'
    ? 'Administrator'
    : 'User'

  return (
    <div className="shell">
      <aside className="sidebar">
        <Brand />

        <div className="nav-heading">MANAGEMENT</div>

        <nav aria-label="Main navigation">
          {sections.map(entry => (
            <button
              key={entry.id}
              className={`nav-item ${section === entry.id ? 'active' : ''}`}
              aria-current={section === entry.id ? 'page' : undefined}
              onClick={() => setSection(entry.id)}
            >
              <entry.icon size={19} />
              <span>{entry.name}</span>
            </button>
          ))}
        </nav>

        <div className="sidebar-footer">
          Self-hosted execution platform
        </div>
      </aside>

      <main className="main">
        <header className="topbar">
          <div className="breadcrumb">Workspace / {sections.find(entry => entry.id === section)?.name}</div>

          <div className="actions">
            <div className="user-chip" title={session.user.issuer}>
              <UserRound size={18} />
              <div>
                <strong>{displayName}</strong>
                <span className={`role-badge ${session.user.role}`}>
                  {roleLabel}
                </span>
              </div>
            </div>

            <button
              className="icon-button"
              aria-label="Sign out"
              title="Sign out"
              disabled={loggingOut}
              onClick={() => void signOut()}
            >
              {loggingOut
                ? <Loader2 size={19} className="spinning" />
                : <LogOut size={19} />}
            </button>

            {themeButton}

            <button
              className="icon-button"
              aria-label="Refresh dashboard"
              disabled={loading}
              onClick={() => setRevision(value => value + 1)}
            >
              <RefreshCw
                size={19}
                className={loading ? 'spinning' : ''}
              />
            </button>
          </div>
        </header>

        <section className="content">
          <div className="page-heading">
            <div>
              <h1>{sections.find(entry => entry.id === section)?.name}</h1>
              <p>
                {section === 'dashboard' && 'Server overview and configured resource limits.'}
                {section === 'workspaces' && 'Isolated execution workspaces, quotas and lifetimes.'}
                {section === 'jobs' && 'Command execution and captured output.'}
                {section === 'files' && 'Browse, edit and upload workspace files.'}
                {section === 'artifacts' && 'Published immutable results and download links.'}
                {section === 'settings' && 'Runtime diagnostics and capability catalog.'}
              </p>
            </div>

            {data && (
              <span className="status online">
                <CheckCircle2 size={16} />
                HTTP server online
              </span>
            )}
          </div>

          {workspaceError && section !== 'dashboard' && (
            <div className="notice error" role="alert">
              <AlertCircle size={20} />
              <div>
                <strong>Cannot load workspaces</strong>
                <p>{workspaceError}</p>
              </div>
            </div>
          )}

          {section === 'dashboard' && (<>
          {signOutError && (
            <div className="notice error" role="alert">
              <ShieldAlert size={20} />
              <div>
                <strong>Sign-out was not confirmed</strong>
                <p>{signOutError}</p>
                <p className="notice-hint">
                  You are still signed in on this page. Retry the sign-out, or
                  close the browser if the server stays unreachable.
                </p>
                <button
                  className="button primary"
                  disabled={loggingOut}
                  onClick={() => void signOut()}
                >
                  {loggingOut ? 'Signing out...' : 'Retry sign-out'}
                </button>
              </div>
            </div>
          )}

          {error && (
            <div className="notice error" role="alert">
              <AlertCircle size={20} />
              <div>
                <strong>Cannot connect to Workspace</strong>
                <p>{error}</p>
              </div>
            </div>
          )}

          {loading && !data && <p role="status">Loading server data...</p>}

          {data && (
            <>
              {!data.readiness.runtime_ready && (
                <div className="notice warning">
                  <AlertCircle size={20} />
                  <div>
                    <strong>Execution runtime is not ready</strong>
                    <p>{data.readiness.reason ?? 'Initialization pending'}</p>
                  </div>
                </div>
              )}

              <div className="metrics">
                {metrics.map(metric => (
                  <article className="card metric" key={metric.label}>
                    <div className="metric-label">
                      {metric.label}
                      <metric.icon size={20} />
                    </div>
                    <strong>{metric.value}</strong>
                    <small>Configured limit</small>
                  </article>
                ))}
              </div>

              <section className="card system">
                <h2>System information</h2>
                <dl>
                  <dt>Product</dt>
                  <dd>{data.info.product}</dd>
                  <dt>Version</dt>
                  <dd>{data.info.version}</dd>
                  <dt>API version</dt>
                  <dd>{data.info.api_version}</dd>
                  <dt>Platform</dt>
                  <dd>
                    {data.info.platform} / {data.info.architecture}
                  </dd>
                  <dt>HTTP health</dt>
                  <dd>{data.health.status}</dd>
                  <dt>Execution runtime</dt>
                  <dd>
                    {data.readiness.runtime_ready ? 'Ready' : 'Not ready'}
                  </dd>
                  <dt>Instance ID</dt>
                  <dd>{data.info.storage.instance_id}</dd>
                  <dt>SQLite version</dt>
                  <dd>{data.info.storage.sqlite_version}</dd>
                  <dt>Database schema</dt>
                  <dd>{data.info.storage.schema_version}</dd>
                  <dt>Journal mode</dt>
                  <dd>{data.info.storage.journal_mode.toUpperCase()}</dd>
                  <dt>Instance created</dt>
                  <dd>
                    {new Date(
                      data.info.storage.created_at_unix * 1000,
                    ).toLocaleString()}
                  </dd>
                  <dt>Maximum active workspaces</dt>
                  <dd>{data.info.resources.max_active_workspaces}</dd>
                </dl>
              </section>
            </>
          )}
          </>)}

          {section === 'workspaces' && (
            <WorkspacesSection
              workspaces={workspaces}
              refreshWorkspaces={refreshWorkspaces}
            />
          )}

          {section === 'jobs' && (
            <JobsSection
              workspaces={workspaces}
              refreshWorkspaces={refreshWorkspaces}
            />
          )}

          {section === 'files' && (
            <FilesSection
              workspaces={workspaces}
              refreshWorkspaces={refreshWorkspaces}
              onPublished={() => setSection('artifacts')}
            />
          )}

          {section === 'artifacts' && (
            <ArtifactsSection onChanged={refreshWorkspaces} />
          )}

          {section === 'settings' && <SettingsSection />}

        </section>
      </main>
    </div>
  )
}

export default App
