import { useCallback, useEffect, useRef, useState } from 'react'
import {
  ChevronRight,
  File as FileIcon,
  Folder,
  FolderOpen,
  RefreshCw,
  Save,
  Upload,
  UploadCloud,
} from 'lucide-react'
import {
  listFiles,
  publishArtifact,
  readFile,
  uploadFile,
  writeFile,
  type FileEntry,
  type Workspace,
} from '../api.ts'
import { useI18n } from '../i18n/provider.tsx'
import {
  Button,
  Card,
  ConfirmDialog,
  EmptyState,
  Field,
  Select,
  Table,
  useToast,
} from '../components/ui.tsx'
import {
  ErrorNotice,
  PageHeader,
  useDescribeError,
  useRefreshEvent,
} from './shared.tsx'

interface OpenFile {
  path: string
  content: string
  sha: string
  eof: boolean
  binary: boolean
}

export function FilesPage({
  workspaces,
  onPublished,
}: {
  workspaces: Workspace[]
  onPublished: () => void
}) {
  const { t, format } = useI18n()
  const describe = useDescribeError()
  const { notify } = useToast()

  const [workspaceId, setWorkspaceId] = useState(workspaces[0]?.id ?? '')
  const [path, setPath] = useState('')
  const [entries, setEntries] = useState<FileEntry[]>([])
  const [file, setFile] = useState<OpenFile | null>(null)
  const [draft, setDraft] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [saving, setSaving] = useState(false)
  const [pendingAction, setPendingAction] = useState<(() => void) | null>(null)
  const uploadInput = useRef<HTMLInputElement>(null)

  const dirty = file !== null && draft !== file.content

  useEffect(() => {
    if (!workspaceId && workspaces[0]) {
      setWorkspaceId(workspaces[0].id)
    }
  }, [workspaces, workspaceId])

  const openDir = useCallback(
    async (workspace: string, directory: string) => {
      if (!workspace) {
        return
      }

      setLoading(true)
      setError(null)

      try {
        const listing = await listFiles(workspace, directory)
        setEntries(listing.entries)
        setPath(directory)
        setFile(null)
        setDraft('')
      } catch (reason) {
        setError(describe(reason))
      } finally {
        setLoading(false)
      }
    },
    [describe],
  )

  useEffect(() => {
    if (workspaceId) {
      void openDir(workspaceId, '')
    }
  }, [workspaceId, openDir])

  useRefreshEvent(() => {
    if (workspaceId) {
      void openDir(workspaceId, path)
    }
  })

  useEffect(() => {
    if (!dirty) {
      return
    }

    const guard = (event: BeforeUnloadEvent) => {
      event.preventDefault()
      event.returnValue = ''
    }

    window.addEventListener('beforeunload', guard)

    return () => window.removeEventListener('beforeunload', guard)
  }, [dirty])

  const guard = (action: () => void) => {
    if (dirty) {
      setPendingAction(() => action)

      return
    }

    action()
  }

  const openFile = async (entry: FileEntry) => {
    if (!workspaceId || entry.kind !== 'file') {
      return
    }

    setLoading(true)
    setError(null)

    try {
      const chunk = await readFile(workspaceId, entry.path)
      const binary = chunk.encoding !== 'utf-8'

      setFile({
        path: entry.path,
        content: chunk.content,
        sha: chunk.sha256,
        eof: chunk.eof,
        binary,
      })
      setDraft(binary ? '' : chunk.content)
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setLoading(false)
    }
  }

  const save = async () => {
    if (!file || !workspaceId || file.binary) {
      return
    }

    setSaving(true)
    setError(null)

    try {
      const result = await writeFile(workspaceId, file.path, draft, file.sha)

      setFile({ ...file, content: draft, sha: result.sha256 })
      notify(t('files.saved', { size: format.bytes(result.size), sha: result.sha256.slice(0, 12) }))
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setSaving(false)
    }
  }

  const publish = async () => {
    if (!file || !workspaceId) {
      return
    }

    try {
      const artifact = await publishArtifact(workspaceId, file.path)

      notify(t('files.published', { name: artifact.name, sha: artifact.sha256.slice(0, 12) }))
      onPublished()
    } catch (reason) {
      setError(describe(reason))
    }
  }

  const upload = async (selected: File | null) => {
    if (!selected || !workspaceId) {
      return
    }

    setLoading(true)
    setError(null)

    try {
      const target = path ? `${path}/${selected.name}` : selected.name
      const result = await uploadFile(workspaceId, target, selected)

      notify(t('files.uploaded', { path: result.path, size: format.bytes(result.size) }))
      await openDir(workspaceId, path)
    } catch (reason) {
      setError(describe(reason))
    } finally {
      setLoading(false)

      if (uploadInput.current) {
        uploadInput.current.value = ''
      }
    }
  }

  const parent = path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : ''
  const workspace = workspaces.find(entry => entry.id === workspaceId)

  return (
    <div className="page-stack">
      <PageHeader
        title={t('files.title')}
        subtitle={t('files.subtitle')}
        actions={
          <Button
            compact
            icon={<RefreshCw size={16} aria-hidden="true" />}
            loading={loading}
            onClick={() => void openDir(workspaceId, path)}
            disabled={!workspaceId}
          >
            {t('common.refresh')}
          </Button>
        }
      />

      {error && <ErrorNotice message={error} onRetry={() => void openDir(workspaceId, path)} />}

      {workspaces.length === 0 ? (
        <Card>
          <EmptyState
            icon={<FolderOpen size={22} />}
            title={t('files.noWorkspaces')}
          >
            {t('files.noWorkspacesHint')}
          </EmptyState>
        </Card>
      ) : (
        <Card>
          <div className="toolbar">
            <Field label={t('files.selectWorkspace')} htmlFor="files-workspace">
              <Select
                id="files-workspace"
                value={workspaceId}
                onChange={event => {
                  const next = event.target.value

                  guard(() => setWorkspaceId(next))
                }}
              >
                {workspaces.map(entry => (
                  <option key={entry.id} value={entry.id}>
                    {entry.name}
                  </option>
                ))}
              </Select>
            </Field>

            <div className="toolbar-actions">
              <Button
                icon={<Upload size={16} aria-hidden="true" />}
                onClick={() => uploadInput.current?.click()}
                disabled={!workspaceId || loading}
              >
                {t('files.upload')}
              </Button>
              <input
                ref={uploadInput}
                type="file"
                hidden
                onChange={event => void upload(event.target.files?.[0] ?? null)}
              />
            </div>
          </div>

          <nav className="breadcrumbs" aria-label={t('files.title')}>
            <button
              type="button"
              className="link-button"
              onClick={() => guard(() => void openDir(workspaceId, ''))}
            >
              {t('files.breadcrumbRoot')}
            </button>
            {path
              .split('/')
              .filter(Boolean)
              .map((part, index, all) => {
                const target = all.slice(0, index + 1).join('/')

                return (
                  <span key={target} className="crumb">
                    <ChevronRight size={14} aria-hidden="true" />
                    <button
                      type="button"
                      className="link-button"
                      onClick={() => guard(() => void openDir(workspaceId, target))}
                    >
                      {part}
                    </button>
                  </span>
                )
              })}
          </nav>

          {path && (
            <Button
              compact
              variant="ghost"
              onClick={() => guard(() => void openDir(workspaceId, parent))}
            >
              ← {t('files.parent')}
            </Button>
          )}

          <Table label={t('files.title')}>
            <thead>
              <tr>
                <th>{t('files.colName')}</th>
                <th>{t('files.colKind')}</th>
                <th>{t('files.colSize')}</th>
                <th>{t('files.colModified')}</th>
              </tr>
            </thead>
            <tbody>
              {entries.map(entry => (
                <tr key={entry.path}>
                  <td>
                    <button
                      type="button"
                      className="file-row-button"
                      onClick={() =>
                        entry.kind === 'dir'
                          ? guard(() => void openDir(workspaceId, entry.path))
                          : guard(() => void openFile(entry))
                      }
                    >
                      {entry.kind === 'dir' ? (
                        <Folder size={16} aria-hidden="true" />
                      ) : (
                        <FileIcon size={16} aria-hidden="true" />
                      )}
                      <span>{entry.name}</span>
                    </button>
                  </td>
                  <td>
                    {entry.kind === 'dir' ? t('files.kindDir') : t('files.kindFile')}
                  </td>
                  <td>{entry.kind === 'file' ? format.bytes(entry.size) : '—'}</td>
                  <td>{format.dateTime(entry.modified_unix)}</td>
                </tr>
              ))}
            </tbody>
          </Table>

          {entries.length === 0 && !loading && (
            <p className="muted">{t('files.emptyDir')}</p>
          )}
        </Card>
      )}

      {file && (
        <Card>
          <div className="logs-header">
            <div>
              <h2 className="mono">{file.path}</h2>
              <p className="muted small">
                {workspace?.name} · SHA-256 {file.sha.slice(0, 16)}…
              </p>
            </div>
            <div className="row">
              <Button
                icon={<UploadCloud size={16} aria-hidden="true" />}
                onClick={() => void publish()}
                disabled={!file.eof || dirty}
                title={t('files.publishHint')}
              >
                {t('common.publish')}
              </Button>
              <Button
                variant="primary"
                icon={<Save size={16} aria-hidden="true" />}
                loading={saving}
                disabled={file.binary || !file.eof || !dirty}
                onClick={() => void save()}
                title={!file.eof ? t('files.largerThanBuffer') : undefined}
              >
                {t('common.save')}
              </Button>
            </div>
          </div>

          {file.binary && (
            <div className="ui-alert ui-alert-info" role="status">
              <div className="ui-alert-body">
                <p className="ui-alert-text">{t('files.binaryNotice')}</p>
              </div>
            </div>
          )}

          {!file.eof && (
            <div className="ui-alert ui-alert-warning" role="status">
              <div className="ui-alert-body">
                <p className="ui-alert-text">{t('files.largerThanBuffer')}</p>
              </div>
            </div>
          )}

          <textarea
            className="file-editor"
            spellCheck={false}
            aria-label={t('files.editorLabel')}
            readOnly={file.binary || !file.eof}
            value={file.binary ? file.content : draft}
            onChange={event => setDraft(event.target.value)}
          />

          {dirty && <p className="muted small">{t('files.unsavedTitle')}</p>}
        </Card>
      )}

      <ConfirmDialog
        open={pendingAction !== null}
        title={t('files.unsavedTitle')}
        body={t('files.unsavedBody', { path: file?.path ?? '' })}
        confirmLabel={t('files.discard')}
        cancelLabel={t('files.keepEditing')}
        destructive
        onCancel={() => setPendingAction(null)}
        onConfirm={() => {
          const action = pendingAction

          setPendingAction(null)
          setFile(null)
          setDraft('')
          action?.()
        }}
      />
    </div>
  )
}
