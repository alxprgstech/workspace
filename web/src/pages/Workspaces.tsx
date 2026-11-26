import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  FolderPlus,
  Pin,
  PinOff,
  Play,
  Plus,
  RefreshCw,
  Square,
  Trash2,
} from 'lucide-react'
import {
  createWorkspace,
  deleteWorkspace,
  loadCapabilities,
  startWorkspace,
  stopWorkspace,
  updateWorkspace,
  workspaceDetail,
  type CapabilityCatalog,
  type Workspace,
  type WorkspaceDetail,
} from '../api.ts'
import { useI18n } from '../i18n/provider.tsx'
import { WORKSPACE_STATES } from '../i18n/index.ts'
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  CopyButton,
  DefinitionList,
  Dialog,
  EmptyState,
  Field,
  IconButton,
  Select,
  Skeleton,
  Table,
  TextInput,
} from '../components/ui.tsx'
import {
  ErrorNotice,
  PageHeader,
  StatusBadge,
  TechnicalId,
  useDescribeError,
  useRefreshEvent,
} from './shared.tsx'

export function WorkspacesPage({
  workspaces,
  active,
  routeWorkspaceId,
  onOpenWorkspace,
  refreshWorkspaces,
}: {
  workspaces: Workspace[]
  active: boolean
  routeWorkspaceId?: string
  onOpenWorkspace: (id?: string) => void
  refreshWorkspaces: () => void
}) {
  const { t, locale, format } = useI18n()
  const describe = useDescribeError()

  const [catalog, setCatalog] = useState<CapabilityCatalog | null>(null)
  const [name, setName] = useState('')
  const [mode, setMode] = useState<'ephemeral' | 'persistent'>('ephemeral')
  const [selected, setSelected] = useState<string[]>(['base'])
  const [creating, setCreating] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [detail, setDetail] = useState<WorkspaceDetail | null>(null)
  const [detailLoading, setDetailLoading] = useState(false)
  const [busyId, setBusyId] = useState<string | null>(null)
  const [pendingDelete, setPendingDelete] = useState<Workspace | null>(null)

  useEffect(() => {
    const controller = new AbortController()

    loadCapabilities(controller.signal)
      .then(setCatalog)
      .catch(reason => {
        if (!controller.signal.aborted) {
          setError(describe(reason))
        }
      })

    return () => controller.abort()
  }, [describe])

  useRefreshEvent(refreshWorkspaces)

  const loadDetail = useCallback(
    async (id: string) => {
      setDetailLoading(true)

      try {
        setDetail(await workspaceDetail(id))
        setError(null)
      } catch (reason) {
        setError(describe(reason))
      } finally {
        setDetailLoading(false)
      }
    },
    [describe],
  )

  useEffect(() => {
    if (routeWorkspaceId) {
      void loadDetail(routeWorkspaceId)
    } else {
      setDetail(null)
    }
  }, [routeWorkspaceId, loadDetail])

  const preparing = workspaces.some(workspace => workspace.state === 'preparing')

  useEffect(() => {
    if (!active || !preparing) {
      return
    }

    const timer = window.setInterval(refreshWorkspaces, 3000)

    return () => window.clearInterval(timer)
  }, [active, preparing, refreshWorkspaces])

  useEffect(() => {
    if (!active || detail?.workspace.state !== 'preparing') {
      return
    }

    const timer = window.setInterval(() => void loadDetail(detail.workspace.id), 3000)

    return () => window.clearInterval(timer)
  }, [active, detail, loadDetail])

  const capabilities = useMemo(
    () => catalog?.modules.filter(module => module.status === 'available') ?? [],
    [catalog],
  )

  const toggle = (id: string) => {
    if (id === 'base') {
      return
    }

    setSelected(current =>
      current.includes(id) ? current.filter(value => value !== id) : [...current, id],
    )
  }

  const run = async (id: string, action: () => Promise<unknown>) => {
    setBusyId(id)
    setError(null)

    try {
      await action()
      refreshWorkspaces()

      if (detail?.workspace.id === id) {
        await loadDetail(id)
      }
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setBusyId(null)
    }
  }

  const create = async () => {
    setCreating(true)
    setError(null)

    try {
      const workspace = await createWorkspace({
        name: name.trim(),
        execution_mode: mode,
        capabilities: selected,
      })

      setName('')
      refreshWorkspaces()
      onOpenWorkspace(workspace.id)
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setCreating(false)
    }
  }

  const confirmDelete = async () => {
    if (!pendingDelete) {
      return
    }

    const workspace = pendingDelete

    setBusyId(workspace.id)

    try {
      await deleteWorkspace(workspace.id)
      setPendingDelete(null)
      refreshWorkspaces()

      if (detail?.workspace.id === workspace.id || routeWorkspaceId === workspace.id) {
        onOpenWorkspace(undefined)
      }
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setBusyId(null)
    }
  }

  const now = Math.floor(Date.now() / 1000)

  return (
    <div className="page-stack">
      <PageHeader
        title={t('workspaces.title')}
        subtitle={t('workspaces.subtitle')}
        actions={
          <Button
            compact
            icon={<RefreshCw size={16} aria-hidden="true" />}
            onClick={refreshWorkspaces}
          >
            {t('common.refresh')}
          </Button>
        }
        status={<Badge tone="neutral">{format.number(workspaces.length)}</Badge>}
      />

      {error && <ErrorNotice message={error} onRetry={refreshWorkspaces} />}

      <Card>
        <div className="section-title">
          <h2>{t('workspaces.createTitle')}</h2>
          <p>{t('workspaces.createLead')}</p>
        </div>

        <div className="form-grid">
          <Field label={t('workspaces.name')} htmlFor="workspace-name">
            <TextInput
              id="workspace-name"
              value={name}
              maxLength={64}
              placeholder={t('workspaces.namePlaceholder')}
              onChange={event => setName(event.target.value)}
            />
          </Field>

          <Field label={t('workspaces.mode')} htmlFor="workspace-mode">
            <Select
              id="workspace-mode"
              value={mode}
              onChange={event => setMode(event.target.value as 'ephemeral' | 'persistent')}
            >
              <option value="ephemeral">{t('workspaces.modeEphemeral')}</option>
              <option value="persistent">{t('workspaces.modePersistent')}</option>
            </Select>
            <small>
              {mode === 'ephemeral'
                ? t('workspaces.modeEphemeralHint')
                : t('workspaces.modePersistentHint')}
            </small>
          </Field>
        </div>

        <fieldset className="capabilities-field">
          <legend>{t('common.capabilities')}</legend>
          <div className="chips">
            {capabilities.map(module => {
              const chosen = selected.includes(module.id)

              return (
                <button
                  key={module.id}
                  type="button"
                  className={`chip ${chosen ? 'is-selected' : ''}`}
                  aria-pressed={chosen}
                  disabled={module.id === 'base'}
                  title={module.description}
                  onClick={() => toggle(module.id)}
                >
                  <span>{module.name}</span>
                  <small>{module.version}</small>
                </button>
              )
            })}
            {capabilities.length === 0 && <span className="muted">{t('common.loading')}</span>}
          </div>
          <small>{t('workspaces.capabilitiesHint')}</small>
        </fieldset>

        <Button
          variant="primary"
          icon={<Plus size={17} aria-hidden="true" />}
          loading={creating}
          disabled={name.trim().length === 0}
          onClick={() => void create()}
        >
          {creating ? t('workspaces.creating') : t('workspaces.create')}
        </Button>
      </Card>

      <Card>
        <div className="section-title">
          <h2>{t('workspaces.listTitle')}</h2>
        </div>

        {workspaces.length === 0 ? (
          <EmptyState icon={<FolderPlus size={22} />} title={t('workspaces.empty')} />
        ) : (
          <Table label={t('workspaces.listTitle')}>
            <thead>
              <tr>
                <th>{t('workspaces.colName')}</th>
                <th>{t('workspaces.colState')}</th>
                <th>{t('workspaces.colMode')}</th>
                <th>{t('workspaces.colCapabilities')}</th>
                <th>{t('workspaces.colTtl')}</th>
                <th>{t('common.actions')}</th>
              </tr>
            </thead>
            <tbody>
              {workspaces.map(workspace => {
                const ttl = Math.max(0, workspace.expires_at_unix - now)

                return (
                  <tr key={workspace.id} className={busyId === workspace.id ? 'is-busy' : undefined}>
                    <td>
                      <button
                        type="button"
                        className="link-button"
                        onClick={() => onOpenWorkspace(workspace.id)}
                      >
                        {workspace.name}
                      </button>
                      <div className="inline-copy muted">
                        <TechnicalId value={workspace.id.slice(0, 8)} label={workspace.id} />
                        <CopyButton value={workspace.id} />
                      </div>
                    </td>
                    <td>
                      <StatusBadge code={workspace.state} table={WORKSPACE_STATES} locale={locale} />
                    </td>
                    <td>{workspace.execution_mode === 'persistent' ? t('workspaces.modePersistent') : t('workspaces.modeEphemeral')}</td>
                    <td className="small">{workspace.capabilities.join(', ')}</td>
                    <td>
                      {workspace.pinned
                        ? t('workspaces.pinnedTtl')
                        : format.duration(ttl)}
                    </td>
                    <td>
                      <div className="row-actions">
                        {workspace.state === 'stopped' || workspace.state === 'failed' ? (
                          <IconButton
                            label={t('common.start')}
                            disabled={busyId === workspace.id}
                            onClick={() => void run(workspace.id, () => startWorkspace(workspace.id))}
                          >
                            <Play size={16} aria-hidden="true" />
                          </IconButton>
                        ) : (
                          <IconButton
                            label={t('common.stop')}
                            disabled={busyId === workspace.id}
                            onClick={() => void run(workspace.id, () => stopWorkspace(workspace.id))}
                          >
                            <Square size={16} aria-hidden="true" />
                          </IconButton>
                        )}
                        <IconButton
                          label={workspace.pinned ? t('common.unpin') : t('common.pin')}
                          disabled={busyId === workspace.id}
                          onClick={() =>
                            void run(workspace.id, () =>
                              updateWorkspace(workspace.id, { pinned: !workspace.pinned }),
                            )
                          }
                        >
                          {workspace.pinned ? (
                            <PinOff size={16} aria-hidden="true" />
                          ) : (
                            <Pin size={16} aria-hidden="true" />
                          )}
                        </IconButton>
                        <IconButton
                          label={t('workspaces.deleteTitle')}
                          tone="danger"
                          disabled={busyId === workspace.id}
                          onClick={() => setPendingDelete(workspace)}
                        >
                          <Trash2 size={16} aria-hidden="true" />
                        </IconButton>
                      </div>
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </Table>
        )}
      </Card>

      <Dialog
        open={detail !== null || (detailLoading && routeWorkspaceId !== undefined)}
        title={detail ? `${detail.workspace.name}` : t('workspaces.detailsTitle')}
        onClose={() => onOpenWorkspace(undefined)}
        size="wide"
        footer={
          detail && (
            <>
              <Button
                onClick={() =>
                  void run(detail.workspace.id, () =>
                    updateWorkspace(detail.workspace.id, { ttl_seconds: 86_400 }),
                  )
                }
                disabled={busyId === detail.workspace.id}
              >
                {t('common.extend')}
              </Button>
              <Button
                icon={<RefreshCw size={16} aria-hidden="true" />}
                onClick={() => void loadDetail(detail.workspace.id)}
                loading={detailLoading}
              >
                {t('common.refresh')}
              </Button>
            </>
          )
        }
      >
        {detail ? (
          <div className="stack">
            <div className="row">
              <StatusBadge
                code={detail.workspace.state}
                table={WORKSPACE_STATES}
                locale={locale}
              />
              {detail.workspace.pinned && <Badge tone="info">{t('common.pin')}</Badge>}
            </div>

            {detail.workspace.error && (
              <div className="ui-alert ui-alert-danger" role="alert">
                <div className="ui-alert-body">
                  <strong>{t('workspaces.preparationError')}</strong>
                  <p className="ui-alert-text">{detail.workspace.error}</p>
                </div>
              </div>
            )}

            <DefinitionList
              rows={[
                {
                  term: t('common.id'),
                  value: (
                    <span className="inline-copy">
                      <TechnicalId value={detail.workspace.id} label={t('common.id')} />
                      <CopyButton value={detail.workspace.id} />
                    </span>
                  ),
                },
                { term: t('workspaces.image'), value: <code className="break">{detail.workspace.image}</code> },
                { term: t('workspaces.network'), value: <code>{detail.workspace.network_mode}</code> },
                {
                  term: t('workspaces.container'),
                  value: detail.workspace.container_name ? (
                    <code>{detail.workspace.container_name}</code>
                  ) : (
                    '—'
                  ),
                },
                {
                  term: t('workspaces.cpuMemory'),
                  value: `${detail.workspace.limits.cpu_millis / 1000} / ${format.bytes(
                    detail.workspace.limits.memory_mib * 1024 * 1024,
                  )}`,
                },
                { term: t('workspaces.processes'), value: format.number(detail.workspace.limits.pids) },
                { term: t('workspaces.ttlRemaining'), value: format.duration(detail.ttl_remaining_seconds) },
                { term: t('common.created'), value: format.dateTime(detail.workspace.created_at_unix) },
                { term: t('common.expires'), value: format.dateTime(detail.workspace.expires_at_unix) },
              ]}
            />

            {detail.usage ? (
              <div className="quota-grid">
                <QuotaBar
                  label={t('workspaces.diskQuota')}
                  used={detail.usage.used_bytes}
                  limit={detail.usage.hard_bytes}
                  render={value => format.bytes(value)}
                />
                <QuotaBar
                  label={t('workspaces.filesQuota')}
                  used={detail.usage.used_files}
                  limit={detail.usage.hard_files}
                  render={value => format.number(value)}
                />
              </div>
            ) : (
              <p className="muted">{t('workspaces.noUsage')}</p>
            )}

            <p className="muted small">{t('workspaces.pinHint')}</p>
          </div>
        ) : (
          <Skeleton lines={5} />
        )}
      </Dialog>

      <ConfirmDialog
        open={pendingDelete !== null}
        title={t('workspaces.deleteTitle')}
        body={t('workspaces.deleteBody', { name: pendingDelete?.name ?? '' })}
        confirmLabel={t('workspaces.deleteConfirm')}
        destructive
        busy={pendingDelete !== null && busyId === pendingDelete.id}
        onCancel={() => setPendingDelete(null)}
        onConfirm={() => void confirmDelete()}
      />
    </div>
  )
}

function QuotaBar({
  label,
  used,
  limit,
  render,
}: {
  label: string
  used: number
  limit: number
  render: (value: number) => string
}) {
  const { t } = useI18n()
  const percent = limit > 0 ? Math.min(100, Math.round((used / limit) * 100)) : 0

  return (
    <div className="quota">
      <div className="quota-head">
        <span>{label}</span>
        <span className="muted">
          {t('common.usedOf', { used: render(used), total: render(limit) })}
        </span>
      </div>
      <div
        className="quota-track"
        role="progressbar"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={percent}
        aria-label={label}
      >
        <span style={{ width: `${percent}%` }} />
      </div>
    </div>
  )
}
