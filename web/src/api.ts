export interface ResourceBudget {
  cpu_millis: number
  memory_mib: number
  storage_mib: number
  max_active_workspaces: number
  max_concurrent_jobs: number
}

export interface ServerInfo {
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

async function request<T>(
  path: string,
  signal: AbortSignal,
  acceptedStatuses = [200],
): Promise<T> {
  const response = await fetch(path, {
    signal,
    cache: 'no-store',
    headers: {
      Accept: 'application/json',
    },
  })

  if (!acceptedStatuses.includes(response.status)) {
    throw new Error(`${path}: HTTP ${response.status}`)
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
