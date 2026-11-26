/**
 * Hash routing.
 *
 * The embedded Rust router serves `/` and `/assets/*` only, so the interface
 * keeps its location in the hash. That preserves back/forward and refresh
 * without a catch-all route that could swallow API, auth, OAuth or artifact
 * paths.
 */
export const PAGES = [
  'dashboard',
  'workspaces',
  'jobs',
  'files',
  'artifacts',
  'settings',
] as const

export type PageId = (typeof PAGES)[number]

export interface Route {
  page: PageId
  /** Workspace opened in the details panel. */
  workspaceId?: string
  /** Workspace filter of the jobs page. */
  jobWorkspaceId?: string
}

export const DEFAULT_ROUTE: Route = { page: 'dashboard' }

export function isPageId(value: string): value is PageId {
  return (PAGES as readonly string[]).includes(value)
}

export function parseRoute(hash: string): Route {
  const raw = hash.startsWith('#') ? hash.slice(1) : hash
  const [pathPart = '', query = ''] = raw.split('?', 2)
  const segments = pathPart.split('/').filter(Boolean).map(decodeURIComponent)
  const params = new URLSearchParams(query)

  const [first, second] = segments

  if (!first) {
    return { ...DEFAULT_ROUTE }
  }

  if (first === 'workspaces') {
    return second ? { page: 'workspaces', workspaceId: second } : { page: 'workspaces' }
  }

  if (isPageId(first)) {
    if (first === 'jobs') {
      const workspaceId = params.get('workspace')

      return workspaceId ? { page: 'jobs', jobWorkspaceId: workspaceId } : { page: 'jobs' }
    }

    return { page: first }
  }

  return { ...DEFAULT_ROUTE }
}

export function routeToHash(route: Route): string {
  switch (route.page) {
    case 'workspaces':
      return route.workspaceId
        ? `#/workspaces/${encodeURIComponent(route.workspaceId)}`
        : '#/workspaces'
    case 'jobs':
      return route.jobWorkspaceId
        ? `#/jobs?workspace=${encodeURIComponent(route.jobWorkspaceId)}`
        : '#/jobs'
    default:
      return `#/${route.page}`
  }
}

export function sameRoute(left: Route, right: Route): boolean {
  return (
    left.page === right.page &&
    left.workspaceId === right.workspaceId &&
    left.jobWorkspaceId === right.jobWorkspaceId
  )
}
