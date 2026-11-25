import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  AlertCircle,
  CheckCircle2,
  Download,
  File as FileIcon,
  Folder,
  Loader2,
  Pin,
  PinOff,
  Play,
  Plus,
  RefreshCw,
  ShieldAlert,
  Square,
  Trash2,
  Upload,
  XCircle,
} from 'lucide-react'
import {
  ApiError,
  artifactLink,
  cancelJob,
  createWorkspace,
  deleteWorkspace,
  jobLogs,
  listArtifacts,
  listFiles,
  listJobs,
  loadCapabilities,
  loadRuntime,
  publishArtifact,
  readFile,
  revokeArtifact,
  startWorkspace,
  stopWorkspace,
  updateWorkspace,
  workspaceDetail,
  uploadFile,
  writeFile,
  type Artifact,
  type CapabilityCatalog,
  type FileEntry,
  type Job,
  type RuntimeStatus,
  type Workspace,
  type WorkspaceDetail,
} from './api'

function describe(reason: unknown): string {
  if (reason instanceof ApiError) {
    return reason.message
  }

  if (reason instanceof Error) {
    return reason.message
  }

  return 'Unexpected error'
}

function when(unix: number | null | undefined): string {
  if (!unix) {
    return '—'
  }

  return new Date(unix * 1000).toLocaleString()
}

function bytes(value: number): string {
  if (value < 1024) {
    return `${value} B`
  }

  if (value < 1024 * 1024) {
    return `${(value / 1024).toFixed(1)} KiB`
  }

  if (value < 1024 * 1024 * 1024) {
    return `${(value / 1024 / 1024).toFixed(1)} MiB`
  }

  return `${(value / 1024 / 1024 / 1024).toFixed(2)} GiB`
}

function errorNotice(message: string, onRetry?: () => void) {
  return (
    <div className="notice error" role="alert">
      <AlertCircle size={20} />
      <div>
        <strong>Operation failed</strong>
        <p>{message}</p>
        {onRetry && (
          <button className="button" onClick={onRetry}>
            Retry
          </button>
        )}
      </div>
    </div>
  )
}

function StateBadge({ state }: { state: string }) {
  const healthy = ['running', 'idle', 'created'].includes(state)
  const failed = ['failed', 'expired'].includes(state)

  return (
    <span className={`badge ${healthy ? 'ok' : failed ? 'bad' : 'pending'}`}>
      {state}
    </span>
  )
}

interface SectionProps {
  workspaces: Workspace[]
  refreshWorkspaces: () => void
}

// ---------------------------------------------------------------------------
// Workspaces
// ---------------------------------------------------------------------------

export function WorkspacesSection({ workspaces, refreshWorkspaces }: SectionProps) {
  const [catalog, setCatalog] = useState<CapabilityCatalog | null>(null)
  const [name, setName] = useState('')
  const [mode, setMode] = useState<'ephemeral' | 'persistent'>('ephemeral')
  const [selected, setSelected] = useState<string[]>(['base'])
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [detail, setDetail] = useState<WorkspaceDetail | null>(null)

  useEffect(() => {
    const controller = new AbortController()

    loadCapabilities(controller.signal)
      .then(setCatalog)
      .catch(reason => setError(describe(reason)))

    return () => controller.abort()
  }, [])

  const preparing = workspaces.some(workspace => workspace.state === 'preparing')

  useEffect(() => {
    if (!preparing) {
      return
    }

    const timer = window.setInterval(refreshWorkspaces, 3000)

    return () => window.clearInterval(timer)
  }, [preparing, refreshWorkspaces])

  const loadDetail = useCallback(async (id: string) => {
    try {
      setDetail(await workspaceDetail(id))
    } catch (reason) {
      setError(describe(reason))
    }
  }, [])

  useEffect(() => {
    if (detail?.workspace.state === 'preparing') {
      const timer = window.setInterval(() => void loadDetail(detail.workspace.id), 3000)

      return () => window.clearInterval(timer)
    }

    return undefined
  }, [detail, loadDetail])

  const capabilities = catalog?.modules.filter(module => module.status === 'available') ?? []

  const toggle = (id: string) => {
    if (id === 'base') {
      return
    }

    setSelected(current =>
      current.includes(id) ? current.filter(value => value !== id) : [...current, id],
    )
  }

  const create = async () => {
    setBusy(true)
    setError(null)

    try {
      const workspace = await createWorkspace({
        name,
        execution_mode: mode,
        capabilities: selected,
      })

      setName('')
      refreshWorkspaces()
      void loadDetail(workspace.id)
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setBusy(false)
    }
  }

  const act = async (action: () => Promise<unknown>) => {
    setError(null)

    try {
      await action()
      refreshWorkspaces()

      if (detail) {
        void loadDetail(detail.workspace.id)
      }
    } catch (reason) {
      setError(describe(reason))
    }
  }

  return (
    <div className="stack">
      {error && errorNotice(error)}

      <section className="card">
        <h2>Create workspace</h2>
        <p className="muted">
          Image preparation runs in the dedicated runtime and may take a few
          minutes; the workspace leaves <code>preparing</code> when it is ready.
        </p>

        <div className="form-grid">
          <label className="field">
            <span>Name</span>
            <input
              className="input"
              value={name}
              maxLength={64}
              onChange={event => setName(event.target.value)}
              placeholder="clipforge"
            />
          </label>

          <label className="field">
            <span>Execution mode</span>
            <select
              className="select"
              value={mode}
              onChange={event => setMode(event.target.value as 'ephemeral' | 'persistent')}
            >
              <option value="ephemeral">ephemeral (container per job)</option>
              <option value="persistent">persistent (container per workspace)</option>
            </select>
          </label>
        </div>

        <div className="field">
          <span>Capabilities</span>
          <div className="chips">
            {capabilities.map(module => (
              <button
                key={module.id}
                type="button"
                className={`chip ${selected.includes(module.id) ? 'selected' : ''}`}
                onClick={() => toggle(module.id)}
                title={module.description}
              >
                {module.name} <small>{module.version}</small>
              </button>
            ))}
            {capabilities.length === 0 && <span className="muted">Loading catalog…</span>}
          </div>
        </div>

        <button
          className="button primary"
          disabled={busy || name.trim().length === 0}
          onClick={() => void create()}
        >
          {busy ? <Loader2 size={18} className="spinning" /> : <Plus size={18} />}
          Create workspace
        </button>
      </section>

      <section className="card">
        <h2>Workspaces</h2>

        {workspaces.length === 0 && <p className="muted">No workspaces yet.</p>}

        {workspaces.length > 0 && (
          <table className="table">
            <thead>
              <tr>
                <th>Name</th>
                <th>State</th>
                <th>Mode</th>
                <th>Capabilities</th>
                <th>TTL</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {workspaces.map(workspace => (
                <tr key={workspace.id}>
                  <td>
                    <button className="link" onClick={() => void loadDetail(workspace.id)}>
                      {workspace.name}
                    </button>
                    <div className="muted small">{workspace.id.slice(0, 8)}</div>
                  </td>
                  <td><StateBadge state={workspace.state} /></td>
                  <td>{workspace.execution_mode}</td>
                  <td className="small">{workspace.capabilities.join(', ')}</td>
                  <td className="small">{when(workspace.expires_at_unix)}</td>
                  <td className="row-actions">
                    {workspace.state === 'stopped' || workspace.state === 'failed' ? (
                      <button
                        className="icon-button"
                        title="Start"
                        onClick={() => void act(() => startWorkspace(workspace.id))}
                      >
                        <Play size={17} />
                      </button>
                    ) : (
                      <button
                        className="icon-button"
                        title="Stop"
                        onClick={() => void act(() => stopWorkspace(workspace.id))}
                      >
                        <Square size={17} />
                      </button>
                    )}

                    <button
                      className="icon-button"
                      title={workspace.pinned ? 'Unpin' : 'Pin (no TTL cleanup)'}
                      onClick={() =>
                        void act(() =>
                          updateWorkspace(workspace.id, { pinned: !workspace.pinned }),
                        )
                      }
                    >
                      {workspace.pinned ? <Pin size={17} /> : <PinOff size={17} />}
                    </button>

                    <button
                      className="icon-button danger"
                      title="Delete workspace"
                      onClick={() => {
                        if (window.confirm(`Delete ${workspace.name}? Files are removed.`)) {
                          void act(async () => {
                            await deleteWorkspace(workspace.id)

                            if (detail?.workspace.id === workspace.id) {
                              setDetail(null)
                            }
                          })
                        }
                      }}
                    >
                      <Trash2 size={17} />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      {detail && (
        <section className="card">
          <h2>
            {detail.workspace.name} <StateBadge state={detail.workspace.state} />
          </h2>

          {detail.workspace.error && (
            <div className="notice error" role="alert">
              <ShieldAlert size={20} />
              <div>
                <strong>Preparation error</strong>
                <p>{detail.workspace.error}</p>
              </div>
            </div>
          )}

          <dl className="details">
            <dt>Image</dt>
            <dd className="small break">{detail.workspace.image}</dd>
            <dt>Network</dt>
            <dd>{detail.workspace.network_mode}</dd>
            <dt>Container</dt>
            <dd>{detail.workspace.container_name ?? '—'}</dd>
            <dt>CPU / memory</dt>
            <dd>
              {detail.workspace.limits.cpu_millis / 1000} cores /{' '}
              {detail.workspace.limits.memory_mib} MiB
            </dd>
            <dt>Processes</dt>
            <dd>{detail.workspace.limits.pids}</dd>
            <dt>TTL remaining</dt>
            <dd>{Math.floor(detail.ttl_remaining_seconds / 60)} min</dd>
            <dt>Disk quota</dt>
            <dd>
              {detail.usage
                ? `${bytes(detail.usage.used_bytes)} of ${bytes(detail.usage.hard_bytes)}`
                : '—'}
            </dd>
            <dt>Files</dt>
            <dd>
              {detail.usage
                ? `${detail.usage.used_files} of ${detail.usage.hard_files}`
                : '—'}
            </dd>
            <dt>Created</dt>
            <dd>{when(detail.workspace.created_at_unix)}</dd>
          </dl>

          <div className="row">
            <button
              className="button"
              onClick={() => void act(() => updateWorkspace(detail.workspace.id, { ttl_seconds: 86_400 }))}
            >
              Extend TTL by 24h
            </button>
            <button
              className="button"
              onClick={() => void loadDetail(detail.workspace.id)}
            >
              <RefreshCw size={17} /> Refresh
            </button>
          </div>
        </section>
      )}
    </div>
  )
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

interface JobsProps extends SectionProps {
  initialWorkspace?: string | null
}

export function JobsSection({ workspaces, initialWorkspace }: JobsProps) {
  const [workspaceId, setWorkspaceId] = useState(initialWorkspace ?? workspaces[0]?.id ?? '')
  const [jobs, setJobs] = useState<Job[]>([])
  const [selected, setSelected] = useState<Job | null>(null)
  const [stream, setStream] = useState<'stdout' | 'stderr'>('stdout')
  const [logs, setLogs] = useState<string>('')
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

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

      if (selected) {
        const current = next.find(job => job.id === selected.id) ?? null
        setSelected(current)
      }
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setLoading(false)
    }
  }, [workspaceId, selected])

  useEffect(() => {
    void reload()
  }, [workspaceId]) // eslint-disable-line react-hooks/exhaustive-deps

  const loadLogs = useCallback(async (job: Job, which: 'stdout' | 'stderr') => {
    try {
      const page = await jobLogs(job.id, which)
      setLogs(page.data || '(empty)')
    } catch (reason) {
      setError(describe(reason))
    }
  }, [])

  useEffect(() => {
    if (selected) {
      void loadLogs(selected, stream)
    }
  }, [selected, stream, loadLogs])

  useEffect(() => {
    if (!jobs.some(job => job.state === 'queued' || job.state === 'running')) {
      return
    }

    const timer = window.setInterval(() => void reload(), 3000)

    return () => window.clearInterval(timer)
  }, [jobs, reload])

  return (
    <div className="stack">
      {error && errorNotice(error)}

      <section className="card">
        <div className="row between">
          <div className="row">
            <label className="field inline">
              <span>Workspace</span>
              <select
                className="select"
                value={workspaceId}
                onChange={event => {
                  setWorkspaceId(event.target.value)
                  setSelected(null)
                }}
              >
                <option value="">All workspaces</option>
                {workspaces.map(workspace => (
                  <option key={workspace.id} value={workspace.id}>
                    {workspace.name}
                  </option>
                ))}
              </select>
            </label>
          </div>

          <button className="button" onClick={() => void reload()} disabled={loading}>
            {loading ? <Loader2 size={17} className="spinning" /> : <RefreshCw size={17} />}
            Refresh
          </button>
        </div>

        {jobs.length === 0 && <p className="muted">No jobs yet. Run a command from ChatGPT or the file editor.</p>}

        {jobs.length > 0 && (
          <table className="table">
            <thead>
              <tr>
                <th>Job</th>
                <th>State</th>
                <th>Command</th>
                <th>Exit</th>
                <th>Duration</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {jobs.map(job => (
                <tr key={job.id} className={selected?.id === job.id ? 'selected' : ''}>
                  <td>
                    <button className="link" onClick={() => setSelected(job)}>
                      {job.id.slice(0, 8)}
                    </button>
                  </td>
                  <td><StateBadge state={job.state} /></td>
                  <td className="small mono">{job.argv.join(' ')}</td>
                  <td>{job.exit_code ?? '—'}</td>
                  <td className="small">
                    {job.started_at_unix
                      ? `${Math.max(
                          0,
                          (job.finished_at_unix ?? Math.floor(Date.now() / 1000)) -
                            job.started_at_unix,
                        )} s`
                      : '—'}
                  </td>
                  <td>
                    {(job.state === 'queued' || job.state === 'running') && (
                      <button
                        className="icon-button"
                        title="Cancel job"
                        onClick={() => void cancelJob(job.id).then(reload).catch(reason => setError(describe(reason)))}
                      >
                        <XCircle size={17} />
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>

      {selected && (
        <section className="card">
          <div className="row between">
            <h2>
              Logs <span className="muted small mono">{selected.id}</span>
            </h2>

            <div className="tabs">
              <button
                className={`tab ${stream === 'stdout' ? 'active' : ''}`}
                onClick={() => setStream('stdout')}
              >
                stdout ({bytes(selected.stdout_bytes)}{selected.stdout_truncated ? ', truncated' : ''})
              </button>
              <button
                className={`tab ${stream === 'stderr' ? 'active' : ''}`}
                onClick={() => setStream('stderr')}
              >
                stderr ({bytes(selected.stderr_bytes)}{selected.stderr_truncated ? ', truncated' : ''})
              </button>
            </div>
          </div>

          {selected.error && <p className="muted small">{selected.error}</p>}

          <pre className="log">{logs}</pre>

          <button className="button" onClick={() => void loadLogs(selected, stream)}>
            <RefreshCw size={17} /> Reload logs
          </button>
        </section>
      )}
    </div>
  )
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

interface FilesProps extends SectionProps {
  onPublished: () => void
}

export function FilesSection({ workspaces, onPublished }: FilesProps) {
  const [workspaceId, setWorkspaceId] = useState(workspaces[0]?.id ?? '')
  const [path, setPath] = useState('')
  const [entries, setEntries] = useState<FileEntry[]>([])
  const [file, setFile] = useState<{ path: string; content: string; sha: string; eof: boolean } | null>(null)
  const [draft, setDraft] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [status, setStatus] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  useEffect(() => {
    if (!workspaceId && workspaces[0]) {
      setWorkspaceId(workspaces[0].id)
    }
  }, [workspaces, workspaceId])

  const openDir = useCallback(async (workspace: string, directory: string) => {
    setLoading(true)
    setError(null)

    try {
      const listing = await listFiles(workspace, directory)
      setEntries(listing.entries)
      setPath(directory)
      setFile(null)
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    if (workspaceId) {
      void openDir(workspaceId, path)
    }
  }, [workspaceId]) // eslint-disable-line react-hooks/exhaustive-deps

  const openFile = async (entry: FileEntry) => {
    if (!workspaceId || entry.kind !== 'file') {
      return
    }

    setLoading(true)
    setError(null)

    try {
      const chunk = await readFile(workspaceId, entry.path)

      setFile({
        path: entry.path,
        content: chunk.content,
        sha: chunk.sha256,
        eof: chunk.eof,
      })
      setDraft(chunk.encoding === 'utf-8' ? chunk.content : '')
      setStatus(
        chunk.encoding === 'utf-8'
          ? null
          : 'Binary file: preview is base64-encoded and editing is disabled.',
      )
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setLoading(false)
    }
  }

  const save = async () => {
    if (!file || !workspaceId) {
      return
    }

    setLoading(true)
    setError(null)

    try {
      const result = await writeFile(workspaceId, file.path, draft, file.sha)

      setFile({ ...file, content: draft, sha: result.sha256 })
      setStatus(`Saved (${bytes(result.size)}, sha256 ${result.sha256.slice(0, 12)}…)`)
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setLoading(false)
    }
  }

  const upload = async (selected: File | null) => {
    if (!selected || !workspaceId) {
      return
    }

    setLoading(true)
    setError(null)

    try {
      const result = await uploadFile(
        workspaceId,
        path ? `${path}/${selected.name}` : selected.name,
        selected,
      )

      setStatus(`Uploaded ${result.path} (${bytes(result.size)})`)
      await openDir(workspaceId, path)
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setLoading(false)
    }
  }

  const publish = async () => {
    if (!file || !workspaceId) {
      return
    }

    try {
      const artifact = await publishArtifact(workspaceId, file.path)

      setStatus(`Published artifact ${artifact.name} (sha256 ${artifact.sha256.slice(0, 12)}…)`)
      onPublished()
    } catch (reason) {
      setError(describe(reason))
    }
  }

  const parent = path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : ''

  return (
    <div className="stack">
      {error && errorNotice(error)}

      {status && (
        <div className="notice warning" role="status">
          <CheckCircle2 size={20} />
          <div><p>{status}</p></div>
        </div>
      )}

      <section className="card">
        <div className="row between">
          <div className="row">
            <label className="field inline">
              <span>Workspace</span>
              <select
                className="select"
                value={workspaceId}
                onChange={event => {
                  setWorkspaceId(event.target.value)
                  setPath('')
                  setFile(null)
                }}
              >
                {workspaces.map(workspace => (
                  <option key={workspace.id} value={workspace.id}>
                    {workspace.name} ({workspace.state})
                  </option>
                ))}
              </select>
            </label>

            <label className="button file-button">
              <Upload size={17} /> Upload
              <input
                type="file"
                hidden
                onChange={event => void upload(event.target.files?.[0] ?? null)}
              />
            </label>

            <button className="button" onClick={() => void openDir(workspaceId, path)} disabled={loading}>
              {loading ? <Loader2 size={17} className="spinning" /> : <RefreshCw size={17} />}
              Refresh
            </button>
          </div>

          <span className="mono small">/{path}</span>
        </div>

        <div className="crumbs">
          <button className="link" onClick={() => void openDir(workspaceId, '')}>
            workspace
          </button>

          {path.split('/').filter(Boolean).map((part, index, all) => {
            const target = all.slice(0, index + 1).join('/')

            return (
              <span key={target}>
                {' / '}
                <button className="link" onClick={() => void openDir(workspaceId, target)}>
                  {part}
                </button>
              </span>
            )
          })}
        </div>

        {path && (
          <button className="link" onClick={() => void openDir(workspaceId, parent)}>
            ↑ parent directory
          </button>
        )}

        <table className="table">
          <thead>
            <tr>
              <th>Name</th>
              <th>Kind</th>
              <th>Size</th>
              <th>Modified</th>
            </tr>
          </thead>
          <tbody>
            {entries.map(entry => (
              <tr key={entry.path}>
                <td>
                  <button
                    className="link file-link"
                    onClick={() =>
                      entry.kind === 'dir'
                        ? void openDir(workspaceId, entry.path)
                        : void openFile(entry)
                    }
                  >
                    {entry.kind === 'dir'
                      ? <Folder size={16} />
                      : <FileIcon size={16} />}
                    {entry.name}
                  </button>
                </td>
                <td>{entry.kind}</td>
                <td>{entry.kind === 'file' ? bytes(entry.size) : '—'}</td>
                <td className="small">{when(entry.modified_unix)}</td>
              </tr>
            ))}
          </tbody>
        </table>

        {entries.length === 0 && !loading && <p className="muted">Directory is empty.</p>}
      </section>

      {file && (
        <section className="card">
          <div className="row between">
            <h2 className="mono small">{file.path}</h2>
            <div className="row">
              <button className="button" onClick={() => void publish()}>
                <Download size={17} /> Publish artifact
              </button>
              <button
                className="button primary"
                onClick={() => void save()}
                disabled={loading || draft === file.content || !file.eof}
                title={!file.eof ? 'File is larger than the editor buffer' : undefined}
              >
                Save
              </button>
            </div>
          </div>

          <textarea
            className="editor"
            spellCheck={false}
            value={draft}
            onChange={event => setDraft(event.target.value)}
          />
        </section>
      )}
    </div>
  )
}

// ---------------------------------------------------------------------------
// Artifacts
// ---------------------------------------------------------------------------

export function ArtifactsSection({ onChanged }: { onChanged: () => void }) {
  const [artifacts, setArtifacts] = useState<Artifact[]>([])
  const [error, setError] = useState<string | null>(null)
  const [status, setStatus] = useState<string | null>(null)

  const reload = useCallback(async () => {
    try {
      setArtifacts(await listArtifacts())
    } catch (reason) {
      setError(describe(reason))
    }
  }, [])

  useEffect(() => {
    void reload()
  }, [reload])

  const link = async (artifact: Artifact) => {
    try {
      const download = await artifactLink(artifact.id)

      setStatus(`Link valid until ${when(download.expires_at_unix)}`)
      window.open(download.url, '_blank', 'noopener')
    } catch (reason) {
      setError(describe(reason))
    }
  }

  const revoke = async (artifact: Artifact) => {
    try {
      await revokeArtifact(artifact.id)
      await reload()
      onChanged()
    } catch (reason) {
      setError(describe(reason))
    }
  }

  return (
    <div className="stack">
      {error && errorNotice(error)}

      {status && (
        <div className="notice warning" role="status">
          <CheckCircle2 size={20} />
          <div><p>{status}</p></div>
        </div>
      )}

      <section className="card">
        <div className="row between">
          <h2>Artifacts</h2>
          <button className="button" onClick={() => void reload()}>
            <RefreshCw size={17} /> Refresh
          </button>
        </div>

        {artifacts.length === 0 && <p className="muted">No published artifacts.</p>}

        {artifacts.length > 0 && (
          <table className="table">
            <thead>
              <tr>
                <th>Name</th>
                <th>Size</th>
                <th>SHA-256</th>
                <th>Expires</th>
                <th>State</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {artifacts.map(artifact => (
                <tr key={artifact.id}>
                  <td>
                    {artifact.name}
                    <div className="muted small mono">{artifact.id.slice(0, 8)}</div>
                  </td>
                  <td>{bytes(artifact.size_bytes)}</td>
                  <td className="mono tiny">{artifact.sha256.slice(0, 16)}…</td>
                  <td className="small">{when(artifact.expires_at_unix)}</td>
                  <td>
                    <span className={`badge ${artifact.revoked ? 'bad' : 'ok'}`}>
                      {artifact.revoked ? 'revoked' : 'active'}
                    </span>
                  </td>
                  <td className="row-actions">
                    <button
                      className="button small"
                      disabled={artifact.revoked}
                      onClick={() => void link(artifact)}
                    >
                      <Download size={16} /> Download
                    </button>
                    <button
                      className="icon-button danger"
                      title="Revoke"
                      disabled={artifact.revoked}
                      onClick={() => void revoke(artifact)}
                    >
                      <Trash2 size={16} />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </div>
  )
}

// ---------------------------------------------------------------------------
// Settings / diagnostics
// ---------------------------------------------------------------------------

export function SettingsSection() {
  const [runtime, setRuntime] = useState<RuntimeStatus | null>(null)
  const [catalog, setCatalog] = useState<CapabilityCatalog | null>(null)
  const [error, setError] = useState<string | null>(null)

  const reload = useCallback(async () => {
    try {
      const [nextRuntime, nextCatalog] = await Promise.all([
        loadRuntime(),
        loadCapabilities(),
      ])

      setRuntime(nextRuntime)
      setCatalog(nextCatalog)
    } catch (reason) {
      setError(describe(reason))
    }
  }, [])

  useEffect(() => {
    void reload()
  }, [reload])

  const services = useMemo(() => catalog?.services ?? [], [catalog])
  const modules = useMemo(() => catalog?.modules ?? [], [catalog])

  return (
    <div className="stack">
      {error && errorNotice(error, () => void reload())}

      <section className="card">
        <div className="row between">
          <h2>Runtime</h2>
          <button className="button" onClick={() => void reload()}>
            <RefreshCw size={17} /> Refresh
          </button>
        </div>

        {runtime && (
          <>
            <div className="row">
              <span className={`badge ${runtime.infrastructure_ready ? 'ok' : 'bad'}`}>
                infrastructure {runtime.infrastructure_ready ? 'ready' : 'not ready'}
              </span>
              <span className={`badge ${runtime.execution_ready ? 'ok' : 'bad'}`}>
                execution {runtime.execution_ready ? 'ready' : 'not ready'}
              </span>
              <span className="muted small">backend: {runtime.backend ?? 'none'}</span>
            </div>

            <table className="table">
              <thead>
                <tr>
                  <th>Check</th>
                  <th>State</th>
                  <th>Detail</th>
                </tr>
              </thead>
              <tbody>
                {runtime.checks.map(check => (
                  <tr key={check.id}>
                    <td className="mono small">{check.id}</td>
                    <td>
                      <span className={`badge ${check.ok ? 'ok' : check.required ? 'bad' : 'pending'}`}>
                        {check.ok ? 'ok' : check.required ? 'fail' : 'info'}
                      </span>
                    </td>
                    <td className="small">{check.detail}</td>
                  </tr>
                ))}
              </tbody>
            </table>

            {runtime.blocking.length > 0 && (
              <div className="notice warning" role="alert">
                <AlertCircle size={20} />
                <div>
                  <strong>Execution is blocked</strong>
                  <ul>
                    {runtime.blocking.map(reason => (
                      <li key={reason}>{reason}</li>
                    ))}
                  </ul>
                </div>
              </div>
            )}
          </>
        )}
      </section>

      <section className="card">
        <h2>Services</h2>
        <ul className="plain-list">
          {services.map(service => (
            <li key={service.id}>
              <span className={`badge ${service.status === 'ready' ? 'ok' : 'pending'}`}>
                {service.status}
              </span>
              <strong>{service.id}</strong>
              <span className="muted small">{service.description}</span>
            </li>
          ))}
        </ul>
      </section>

      <section className="card">
        <h2>Capability modules</h2>
        <ul className="plain-list">
          {modules.map(module => (
            <li key={module.id}>
              <span className={`badge ${module.status === 'available' ? 'ok' : 'pending'}`}>
                {module.status}
              </span>
              <strong>{module.name}</strong>
              <span className="muted small">
                {module.version} · {module.tools.join(', ')}
              </span>
            </li>
          ))}
        </ul>
      </section>
    </div>
  )
}
