export interface ResourceBudget {
  cpu_millis: number
  memory_mib: number
  storage_mib: number
  max_active_workspaces: number
  max_concurrent_jobs: number
}

export interface StorageInfo {
  instance_id: string
  schema_version: number
  sqlite_version: string
  journal_mode: string
  created_at_unix: number
}

export interface ServerInfo {
  storage: StorageInfo
  product: string
  version: string
  api_version: string
  platform: string
  architecture: string
  runtime_ready: boolean
  resources: ResourceBudget
}

export interface Readiness {
  status: string
  runtime_ready: boolean
  reason?: string
}

export interface DashboardData {
  info: ServerInfo
  readiness: Readiness
  health: { status: string }
}

export interface SessionUser {
  id: string
  issuer: string
  subject: string
  username: string | null
  role: string
  is_enabled: boolean
}

export interface SessionInfo {
  user: SessionUser
  expires_at_unix: number
  access_expires_at_unix: number
}

export class ApiError extends Error {
  readonly status: number

  constructor(status: number, path: string, message?: string) {
    super(message ?? `${path}: HTTP ${status}`)
    this.name = 'ApiError'
    this.status = status
  }
}

async function request<T>(
  path: string,
  signal: AbortSignal,
  acceptedStatuses = [200],
): Promise<T> {
  const response = await fetch(path, {
    signal,
    cache: 'no-store',
    credentials: 'same-origin',
    headers: {
      Accept: 'application/json',
    },
  })

  if (!acceptedStatuses.includes(response.status)) {
    throw new ApiError(response.status, path)
  }

  return await response.json() as T
}

export async function loadDashboard(
  signal: AbortSignal,
): Promise<DashboardData> {
  const [info, readiness, health] = await Promise.all([
    request<ServerInfo>('/api/v1/server/info', signal),
    request<Readiness>('/ready', signal, [200, 503]),
    request<{ status: string }>('/health', signal),
  ])

  return { info, readiness, health }
}

/**
 * Resolves the current web session.
 *
 * Returns null for an anonymous browser (HTTP 401) and throws an
 * ApiError for HTTP 503 so the UI can wait without signing the user out.
 */
export async function loadSession(
  signal: AbortSignal,
): Promise<SessionInfo | null> {
  const response = await fetch('/auth/me', {
    signal,
    cache: 'no-store',
    credentials: 'same-origin',
    headers: {
      Accept: 'application/json',
    },
  })

  if (response.status === 401) {
    return null
  }

  if (response.status !== 200) {
    throw new ApiError(response.status, '/auth/me')
  }

  return await response.json() as SessionInfo
}

function cookie(name: string): string | null {
  for (const pair of document.cookie.split(';')) {
    const [key, ...value] = pair.trim().split('=')

    if (key === name) {
      return value.join('=') || null
    }
  }

  return null
}

export async function logout(): Promise<void> {
  const headers: Record<string, string> = {
    Accept: 'application/json',
  }

  const csrf = cookie('__Host-workspace-csrf')

  if (csrf) {
    headers['x-workspace-csrf'] = csrf
  }

  const response = await fetch('/auth/logout', {
    method: 'POST',
    credentials: 'same-origin',
    cache: 'no-store',
    headers,
  })

  if (response.status !== 204 && response.status !== 401) {
    throw new ApiError(response.status, '/auth/logout')
  }
}

// ---------------------------------------------------------------------------
// Management API (stage 009-012)
// ---------------------------------------------------------------------------

export interface Limits {
  cpu_millis: number
  memory_mib: number
  pids: number
  disk_mib: number
  files: number
  max_job_seconds: number
}

export interface Workspace {
  id: string
  owner_id: string
  name: string
  execution_mode: 'ephemeral' | 'persistent'
  state: string
  capabilities: string[]
  image: string
  network_mode: string
  limits: Limits
  project_id: number
  container_name: string | null
  created_at_unix: number
  updated_at_unix: number
  last_activity_unix: number
  ttl_seconds: number
  expires_at_unix: number
  pinned: boolean
  error: string | null
}

export interface QuotaSnapshot {
  used_bytes: number
  used_files: number
  hard_bytes: number
  hard_files: number
}

export interface WorkspaceDetail {
  workspace: Workspace
  usage: QuotaSnapshot | null
  ttl_remaining_seconds: number
}

export interface Job {
  id: string
  workspace_id: string
  state: string
  argv: string[]
  cwd: string
  exit_code: number | null
  created_at_unix: number
  started_at_unix: number | null
  finished_at_unix: number | null
  timeout_seconds: number
  error: string | null
  stdout_bytes: number
  stderr_bytes: number
  stdout_truncated: boolean
  stderr_truncated: boolean
}

export interface JobLogPage {
  job_id: string
  stream: string
  offset: number
  next_offset: number
  total_bytes: number
  truncated: boolean
  data: string
}

export interface FileEntry {
  path: string
  name: string
  kind: string
  size: number
  modified_unix: number | null
}

export interface FileListing {
  entries: FileEntry[]
  truncated: boolean
}

export interface FileChunk {
  path: string
  size: number
  offset: number
  read: number
  next_offset: number
  eof: boolean
  sha256: string
  encoding: 'utf-8' | 'base64'
  content: string
}

export interface Artifact {
  id: string
  workspace_id: string | null
  name: string
  size_bytes: number
  media_type: string
  sha256: string
  created_at_unix: number
  expires_at_unix: number
  revoked: boolean
}

export interface DownloadLink {
  artifact_id: string
  url: string
  expires_at_unix: number
  sha256: string
  size_bytes: number
}

export interface StatusCheck {
  id: string
  required: boolean
  ok: boolean
  detail: string
}

export interface RuntimeStatus {
  configured: boolean
  backend: string | null
  platform: string
  infrastructure_ready: boolean
  execution_ready: boolean
  readiness?: 'ready' | 'degraded' | 'unavailable' | 'unconfigured'
  blocking: string[]
  checks: StatusCheck[]
}

export interface CapabilityModule {
  id: string
  name: string
  description: string
  version: string
  tools: string[]
  depends_on: string[]
  status: string
}

export interface CapabilityCatalog {
  schema_version: number
  services: { id: string; status: string; description: string }[]
  modules: CapabilityModule[]
  runtime_blocking: string[]
}

function csrfToken(): string | null {
  return cookie('__Host-workspace-csrf')
}

interface RequestOptions {
  method?: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE'
  body?: unknown
  signal?: AbortSignal
  acceptedStatuses?: number[]
}

async function jsonRequest<T>(
  path: string,
  options: RequestOptions = {},
): Promise<T> {
  const method = options.method ?? 'GET'
  const headers: Record<string, string> = { Accept: 'application/json' }

  if (options.body !== undefined) {
    headers['Content-Type'] = 'application/json'
  }

  if (method !== 'GET') {
    const token = csrfToken()

    if (token) {
      headers['x-workspace-csrf'] = token
    }
  }

  const response = await fetch(path, {
    method,
    signal: options.signal,
    cache: 'no-store',
    credentials: 'same-origin',
    headers,
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  })

  const accepted = options.acceptedStatuses ?? [200]

  if (!accepted.includes(response.status)) {
    let message = `${path}: HTTP ${response.status}`

    try {
      const payload = await response.json() as { message?: string; error?: string }

      if (payload.message) {
        message = payload.message
      } else if (payload.error) {
        message = payload.error
      }
    } catch {
      // keep the generic message
    }

    throw new ApiError(response.status, path, message)
  }

  if (response.status === 204) {
    return undefined as T
  }

  return await response.json() as T
}

export async function listWorkspaces(signal?: AbortSignal): Promise<Workspace[]> {
  const payload = await jsonRequest<{ workspaces: Workspace[] }>('/api/v1/workspaces', { signal })

  return payload.workspaces
}

export async function createWorkspace(input: {
  name: string
  execution_mode: 'ephemeral' | 'persistent'
  capabilities: string[]
}): Promise<Workspace> {
  return await jsonRequest<Workspace>('/api/v1/workspaces', {
    method: 'POST',
    body: input,
    acceptedStatuses: [201],
  })
}

export async function workspaceDetail(
  id: string,
  signal?: AbortSignal,
): Promise<WorkspaceDetail> {
  return await jsonRequest<WorkspaceDetail>(`/api/v1/workspaces/${id}`, { signal })
}

export async function updateWorkspace(
  id: string,
  patch: { name?: string; ttl_seconds?: number; pinned?: boolean; execution_mode?: string },
): Promise<Workspace> {
  return await jsonRequest<Workspace>(`/api/v1/workspaces/${id}`, {
    method: 'PATCH',
    body: patch,
  })
}

export async function stopWorkspace(id: string): Promise<Workspace> {
  return await jsonRequest<Workspace>(`/api/v1/workspaces/${id}/stop`, { method: 'POST' })
}

export async function startWorkspace(id: string): Promise<Workspace> {
  return await jsonRequest<Workspace>(`/api/v1/workspaces/${id}/start`, { method: 'POST' })
}

export async function deleteWorkspace(id: string): Promise<void> {
  await jsonRequest<void>(`/api/v1/workspaces/${id}`, {
    method: 'DELETE',
    acceptedStatuses: [204],
  })
}

export async function listJobs(
  workspaceId?: string,
  signal?: AbortSignal,
): Promise<Job[]> {
  const query = workspaceId ? `?workspace_id=${encodeURIComponent(workspaceId)}` : ''
  const payload = await jsonRequest<{ jobs: Job[] }>(`/api/v1/jobs${query}`, { signal })

  return payload.jobs
}

export async function jobLogs(
  id: string,
  stream: 'stdout' | 'stderr',
  signal?: AbortSignal,
): Promise<JobLogPage> {
  return await jsonRequest<JobLogPage>(
    `/api/v1/jobs/${id}/logs?stream=${stream}&limit=262144`,
    { signal },
  )
}

export async function cancelJob(id: string): Promise<Job> {
  return await jsonRequest<Job>(`/api/v1/jobs/${id}/cancel`, { method: 'POST' })
}

export async function listFiles(
  workspaceId: string,
  path: string,
  signal?: AbortSignal,
): Promise<FileListing> {
  return await jsonRequest<FileListing>(
    `/api/v1/workspaces/${workspaceId}/files?path=${encodeURIComponent(path)}&depth=1&limit=1000`,
    { signal },
  )
}

export async function readFile(
  workspaceId: string,
  path: string,
  signal?: AbortSignal,
): Promise<FileChunk> {
  return await jsonRequest<FileChunk>(
    `/api/v1/workspaces/${workspaceId}/file?path=${encodeURIComponent(path)}&length=1048576`,
    { signal },
  )
}

export async function writeFile(
  workspaceId: string,
  path: string,
  content: string,
  expectedSha256?: string,
): Promise<{ path: string; size: number; sha256: string }> {
  return await jsonRequest(`/api/v1/workspaces/${workspaceId}/file`, {
    method: 'PUT',
    body: { path, content, expected_sha256: expectedSha256 },
  })
}

export async function uploadFile(
  workspaceId: string,
  path: string,
  file: File,
): Promise<{ path: string; size: number; sha256: string }> {
  const headers: Record<string, string> = { Accept: 'application/json' }
  const token = csrfToken()

  if (token) {
    headers['x-workspace-csrf'] = token
  }

  const response = await fetch(
    `/api/v1/workspaces/${workspaceId}/uploads?path=${encodeURIComponent(path)}`,
    {
      method: 'POST',
      credentials: 'same-origin',
      headers,
      body: file,
    },
  )

  if (!response.ok) {
    throw new ApiError(response.status, 'upload')
  }

  return await response.json() as { path: string; size: number; sha256: string }
}

export async function listArtifacts(signal?: AbortSignal): Promise<Artifact[]> {
  const payload = await jsonRequest<{ artifacts: Artifact[] }>('/api/v1/artifacts', { signal })

  return payload.artifacts
}

export async function publishArtifact(
  workspaceId: string,
  path: string,
  name?: string,
): Promise<Artifact> {
  return await jsonRequest<Artifact>(`/api/v1/workspaces/${workspaceId}/artifacts`, {
    method: 'POST',
    body: { path, name },
    acceptedStatuses: [201],
  })
}

export async function artifactLink(id: string): Promise<DownloadLink> {
  return await jsonRequest<DownloadLink>(`/api/v1/artifacts/${id}/link`, { method: 'POST' })
}

export async function revokeArtifact(id: string): Promise<Artifact> {
  return await jsonRequest<Artifact>(`/api/v1/artifacts/${id}/revoke`, { method: 'POST' })
}

export async function loadRuntime(signal?: AbortSignal): Promise<RuntimeStatus> {
  return await jsonRequest<RuntimeStatus>('/api/v1/runtime', { signal })
}

export async function loadCapabilities(signal?: AbortSignal): Promise<CapabilityCatalog> {
  return await jsonRequest<CapabilityCatalog>('/api/v1/capabilities', { signal })
}