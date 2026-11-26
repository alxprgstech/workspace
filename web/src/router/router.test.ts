import { test } from 'node:test'
import assert from 'node:assert/strict'
import { parseRoute, PAGES, routeToHash, sameRoute } from './core.ts'

test('empty or unknown hashes fall back to the dashboard', () => {
  assert.deepEqual(parseRoute(''), { page: 'dashboard' })
  assert.deepEqual(parseRoute('#/'), { page: 'dashboard' })
  assert.deepEqual(parseRoute('#/nope'), { page: 'dashboard' })
})

test('every page round-trips through the hash', () => {
  for (const page of PAGES) {
    const route = { page }
    assert.deepEqual(parseRoute(routeToHash(route)), route)
  }
})

test('workspace details and job filters survive the hash', () => {
  const detail = parseRoute('#/workspaces/550e8400-e29b-41d4-a716-446655440000')

  assert.equal(detail.page, 'workspaces')
  assert.equal(detail.workspaceId, '550e8400-e29b-41d4-a716-446655440000')

  const jobs = parseRoute('#/jobs?workspace=abc%20123')

  assert.equal(jobs.page, 'jobs')
  assert.equal(jobs.jobWorkspaceId, 'abc 123')
  assert.equal(parseRoute(routeToHash(jobs)).jobWorkspaceId, 'abc 123')
})

test('route comparison ignores ordering but not values', () => {
  assert.ok(sameRoute({ page: 'jobs' }, { page: 'jobs' }))
  assert.ok(!sameRoute({ page: 'jobs' }, { page: 'files' }))
  assert.ok(!sameRoute({ page: 'workspaces' }, { page: 'workspaces', workspaceId: 'x' }))
})
