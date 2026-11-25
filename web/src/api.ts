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

  constructor(status: number, path: string) {
    super(`${path}: HTTP ${status}`)
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
