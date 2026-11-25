import { useEffect, useState } from 'react'
import {
  Activity,
  AlertCircle,
  Blocks,
  CheckCircle2,
  Cpu,
  Database,
  Folder,
  HardDrive,
  LayoutDashboard,
  MemoryStick,
  Moon,
  Plug,
  RefreshCw,
  Settings,
  Sun,
  Terminal,
} from 'lucide-react'
import { loadDashboard, type DashboardData } from './api'

const sections = [
  { name: 'Dashboard', icon: LayoutDashboard, enabled: true },
  { name: 'Workspaces', icon: Folder, enabled: false },
  { name: 'Jobs', icon: Terminal, enabled: false },
  { name: 'Artifacts', icon: Database, enabled: false },
  { name: 'Integrations', icon: Plug, enabled: false },
  { name: 'Settings', icon: Settings, enabled: false },
]

function App() {
  const [dark, setDark] = useState(
    () => window.matchMedia('(prefers-color-scheme: dark)').matches,
  )

  const [data, setData] = useState<DashboardData | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(true)
  const [revision, setRevision] = useState(0)

  useEffect(() => {
    document.documentElement.dataset.theme = dark ? 'dark' : 'light'
  }, [dark])

  useEffect(() => {
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
        if (!controller.signal.aborted) {
          setData(null)
          setError(
            reason instanceof Error ? reason.message : String(reason),
          )
        }
      })
      .finally(() => {
        if (!controller.signal.aborted) {
          setLoading(false)
        }
      })

    return () => controller.abort()
  }, [revision])

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

  return (
    <div className="shell">
      <aside className="sidebar">
        <div className="brand">
          <div className="brand-icon">
            <Blocks size={25} />
          </div>
          <div>
            <strong>ALXPRGS</strong>
            <span>Workspace</span>
          </div>
        </div>

        <div className="nav-heading">MANAGEMENT</div>

        <nav aria-label="Main navigation">
          {sections.map(section => (
            <button
              key={section.name}
              className={`nav-item ${section.enabled ? 'active' : ''}`}
              disabled={!section.enabled}
              aria-current={section.enabled ? 'page' : undefined}
              title={!section.enabled ? 'Not implemented yet' : undefined}
            >
              <section.icon size={19} />
              <span>{section.name}</span>
            </button>
          ))}
        </nav>

        <div className="sidebar-footer">
          Self-hosted execution platform
        </div>
      </aside>

      <main className="main">
        <header className="topbar">
          <div className="breadcrumb">Workspace / Dashboard</div>

          <div className="actions">
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

            <button
              className="icon-button"
              aria-label="Toggle color theme"
              onClick={() => setDark(value => !value)}
            >
              {dark ? <Sun size={20} /> : <Moon size={20} />}
            </button>
          </div>
        </header>

        <section className="content">
          <div className="page-heading">
            <div>
              <h1>Dashboard</h1>
              <p>Server overview and configured resource limits.</p>
            </div>

            {data && (
              <span className="status online">
                <CheckCircle2 size={16} />
                HTTP server online
              </span>
            )}
          </div>

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
        </section>
      </main>
    </div>
  )
}

export default App
