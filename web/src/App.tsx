import { useCallback, useEffect, useMemo, useState } from 'react'
import { AlertCircle, ShieldCheck } from 'lucide-react'
import {
  ApiError,
  listWorkspaces,
  loadDashboard,
  loadRuntime,
  loadSession,
  logout,
  type DashboardData,
  type RuntimeStatus,
  type SessionInfo,
  type Workspace,
} from './api.ts'
import { I18nProvider, useI18n } from './i18n/provider.tsx'
import type { MessageKey } from './i18n/index.ts'
import { ToastProvider } from './components/ui.tsx'
import { AppLayout } from './components/Layout.tsx'
import { CommandPalette } from './components/CommandPalette.tsx'
import { useTheme } from './theme/useTheme.ts'
import { useRoute } from './router/useRoute.ts'
import type { PageId } from './router/core.ts'
import { LoginPage, type AuthPhase } from './pages/Login.tsx'
import { DashboardPage } from './pages/Dashboard.tsx'
import { WorkspacesPage } from './pages/Workspaces.tsx'
import { JobsPage } from './pages/Jobs.tsx'
import { FilesPage } from './pages/Files.tsx'
import { ArtifactsPage } from './pages/Artifacts.tsx'
import { SettingsPage } from './pages/Settings.tsx'
import { UpdatesPage } from './pages/Updates.tsx'
import { dispatchRefresh } from './pages/shared.tsx'

const PAGE_TITLES: Record<PageId, MessageKey> = {
  dashboard: 'nav.dashboard',
  workspaces: 'nav.workspaces',
  jobs: 'nav.jobs',
  files: 'nav.files',
  artifacts: 'nav.artifacts',
  settings: 'nav.settings',
  updates: 'nav.updates',
}

function WorkspaceApp() {
  const { t } = useI18n()
  const theme = useTheme()
  const [route, navigate] = useRoute()

  const [phase, setPhase] = useState<AuthPhase>('checking')
  const [session, setSession] = useState<SessionInfo | null>(null)
  const [authNotice, setAuthNotice] = useState<string | null>(null)
  const [fault, setFault] = useState<string | null>(null)
  const [check, setCheck] = useState(0)
  const [loggingOut, setLoggingOut] = useState(false)
  const [signOutError, setSignOutError] = useState<string | null>(null)
  const [commandsOpen, setCommandsOpen] = useState(false)

  const [dashboard, setDashboard] = useState<DashboardData | null>(null)
  const [runtime, setRuntime] = useState<RuntimeStatus | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [revision, setRevision] = useState(0)

  const [workspaces, setWorkspaces] = useState<Workspace[]>([])
  const [workspaceError, setWorkspaceError] = useState<string | null>(null)
  const [visited, setVisited] = useState<Set<PageId>>(() => new Set(['dashboard']))

  useEffect(() => {
    const params = new URLSearchParams(window.location.search)
    const notice = params.get('auth_error')

    if (notice) {
      setAuthNotice(notice)
    }

    if (window.location.search) {
      window.history.replaceState(null, '', `${window.location.pathname}${window.location.hash}`)
    }
  }, [])

  useEffect(() => {
    setVisited(current => {
      if (current.has(route.page)) {
        return current
      }

      const next = new Set(current)

      next.add(route.page)

      return next
    })
  }, [route.page])

  useEffect(() => {
    document.title = `${t(PAGE_TITLES[route.page])} · ${t('app.title')}`
  }, [route.page, t])

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

          setFault(reason instanceof Error ? reason.message : String(reason))
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
          setDashboard(result)
        }
      })
      .catch((reason: unknown) => {
        if (controller.signal.aborted) {
          return
        }

        if (reason instanceof ApiError && reason.status === 401) {
          setSession(null)
          setDashboard(null)
          setAuthNotice('session_expired')
          setPhase('anonymous')

          return
        }

        setDashboard(null)
        setError(reason instanceof Error ? reason.message : String(reason))
      })
      .finally(() => {
        if (!controller.signal.aborted) {
          setLoading(false)
        }
      })

    loadRuntime(controller.signal)
      .then(result => {
        if (!controller.signal.aborted) {
          setRuntime(result)
        }
      })
      .catch(() => {
        if (!controller.signal.aborted) {
          setRuntime(null)
        }
      })

    return () => controller.abort()
  }, [phase, revision])

  const refreshWorkspaces = useCallback(() => {
    listWorkspaces()
      .then(next => {
        setWorkspaces(next)
        setWorkspaceError(null)
      })
      .catch((reason: unknown) => {
        setWorkspaceError(reason instanceof Error ? reason.message : t('error.loadFailed'))
      })
  }, [t])

  useEffect(() => {
    if (phase === 'authenticated') {
      refreshWorkspaces()
    }
  }, [phase, refreshWorkspaces])

  useEffect(() => {
    window.addEventListener('workspace:refresh', refreshWorkspaces)

    return () => window.removeEventListener('workspace:refresh', refreshWorkspaces)
  }, [refreshWorkspaces])

  const refreshAll = useCallback(() => {
    setRevision(value => value + 1)
    refreshWorkspaces()
    dispatchRefresh()
  }, [refreshWorkspaces])

  useEffect(() => {
    const shortcut = (event: KeyboardEvent) => {
      if (
        event.key.toLowerCase() === 'k' &&
        (event.ctrlKey || event.metaKey) &&
        !event.altKey
      ) {
        event.preventDefault()
        setCommandsOpen(value => !value)
      }
    }
    const open = () => setCommandsOpen(true)

    document.addEventListener('keydown', shortcut)
    window.addEventListener('workspace:commands', open)

    return () => {
      document.removeEventListener('keydown', shortcut)
      window.removeEventListener('workspace:commands', open)
    }
  }, [])

  const signOut = useCallback(async () => {
    setLoggingOut(true)
    setSignOutError(null)

    try {
      await logout()

      setSession(null)
      setDashboard(null)
      setRuntime(null)
      setWorkspaces([])
      setError(null)
      setAuthNotice(null)
      setPhase('anonymous')
    } catch (reason) {
      setSignOutError(
        reason instanceof ApiError
          ? `${reason.message} (HTTP ${reason.status})`
          : reason instanceof Error
            ? reason.message
            : t('error.unexpected'),
      )
    } finally {
      setLoggingOut(false)
    }
  }, [t])

  const goToPage = useCallback(
    (page: PageId) => {
      navigate({ page })
      document.getElementById('main')?.focus()
    },
    [navigate],
  )

  const openWorkspace = useCallback(
    (workspaceId?: string) => {
      navigate(workspaceId ? { page: 'workspaces', workspaceId } : { page: 'workspaces' })
    },
    [navigate],
  )

  const setJobFilter = useCallback(
    (workspaceId?: string) => {
      navigate(workspaceId ? { page: 'jobs', jobWorkspaceId: workspaceId } : { page: 'jobs' })
    },
    [navigate],
  )

  const version = dashboard?.info.version ?? '0.1.0'

  if (phase !== 'authenticated' || !session) {
    return (
      <LoginPage
        phase={phase}
        authNotice={authNotice}
        fault={fault}
        theme={theme}
        onRetry={() => setCheck(value => value + 1)}
      />
    )
  }

  return (
    <>
      <a className="skip-link" href="#main">
        {t('nav.label')}
      </a>
      <AppLayout
        route={route}
        navigate={goToPage}
        theme={theme}
        session={session}
        version={version}
        signingOut={loggingOut}
        onSignOut={() => void signOut()}
        onOpenCommands={() => setCommandsOpen(true)}
      >
        {signOutError && (
          <div className="ui-alert ui-alert-danger" role="alert">
            <span className="ui-alert-icon">
              <AlertCircle size={18} aria-hidden="true" />
            </span>
            <div className="ui-alert-body">
              <strong>{t('auth.signOutErrorTitle')}</strong>
              <p className="ui-alert-text">{t('auth.signOutError')}</p>
              <p className="ui-alert-text technical-detail">{signOutError}</p>
              <button
                type="button"
                className="ui-button ui-secondary ui-compact"
                disabled={loggingOut}
                onClick={() => void signOut()}
              >
                <ShieldCheck size={15} aria-hidden="true" />
                {t('auth.retrySignOut')}
              </button>
            </div>
          </div>
        )}

        {workspaceError && (
          <div className="ui-alert ui-alert-warning" role="alert">
            <div className="ui-alert-body">
              <strong>{t('error.cannotLoadWorkspaces')}</strong>
              <p className="ui-alert-text">{workspaceError}</p>
            </div>
          </div>
        )}

        <div className="page-host" hidden={route.page !== 'dashboard'}>
          <DashboardPage
            data={dashboard}
            runtime={runtime}
            loading={loading}
            error={error}
            onRefresh={refreshAll}
          />
        </div>

        {visited.has('workspaces') && (
          <div className="page-host" hidden={route.page !== 'workspaces'}>
            <WorkspacesPage
              workspaces={workspaces}
              active={route.page === 'workspaces'}
              routeWorkspaceId={route.workspaceId}
              onOpenWorkspace={openWorkspace}
              refreshWorkspaces={refreshWorkspaces}
            />
          </div>
        )}

        {visited.has('jobs') && (
          <div className="page-host" hidden={route.page !== 'jobs'}>
            <JobsPage
              workspaces={workspaces}
              active={route.page === 'jobs'}
              routeWorkspaceId={route.jobWorkspaceId}
              onFilterChange={setJobFilter}
            />
          </div>
        )}

        {visited.has('files') && (
          <div className="page-host" hidden={route.page !== 'files'}>
            <FilesPage
              workspaces={workspaces}
              onPublished={() => navigate({ page: 'artifacts' })}
            />
          </div>
        )}

        {visited.has('artifacts') && (
          <div className="page-host" hidden={route.page !== 'artifacts'}>
            <ArtifactsPage active={route.page === 'artifacts'} />
          </div>
        )}

        {visited.has('settings') && (
          <div className="page-host" hidden={route.page !== 'settings'}>
            <SettingsPage theme={theme} active={route.page === 'settings'} />
          </div>
        )}

        {visited.has('updates') && (
          <div className="page-host" hidden={route.page !== 'updates'}>
            <UpdatesPage
              active={route.page === 'updates'}
              isAdmin={session.user.role === 'admin'}
            />
          </div>
        )}
      </AppLayout>

      <CommandPalette
        open={commandsOpen}
        onClose={() => setCommandsOpen(false)}
        navigate={goToPage}
        theme={theme}
        onRefresh={refreshAll}
        isAdmin={session.user.role === 'admin'}
      />
    </>
  )
}

export default function App() {
  const memoized = useMemo(
    () => (
      <I18nProvider>
        <ToastProvider>
          <WorkspaceApp />
        </ToastProvider>
      </I18nProvider>
    ),
    [],
  )

  return memoized
}
