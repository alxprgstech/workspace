import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useId,
  useRef,
  useState,
  type ButtonHTMLAttributes,
  type ReactNode,
} from 'react'
import { AlertCircle, Check, CheckCircle2, Copy, Info, Loader2, X } from 'lucide-react'
import { useI18n } from '../i18n/provider.tsx'

export function cx(...values: Array<string | false | null | undefined>): string {
  return values.filter(Boolean).join(' ')
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

type Variant = 'primary' | 'secondary' | 'ghost' | 'danger'

interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: Variant
  compact?: boolean
  loading?: boolean
  icon?: ReactNode
}

export function Button({
  variant = 'secondary',
  compact = false,
  loading = false,
  icon,
  children,
  className,
  disabled,
  type = 'button',
  ...rest
}: ButtonProps) {
  return (
    <button
      {...rest}
      type={type}
      className={cx('ui-button', `ui-${variant}`, compact && 'ui-compact', className)}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
    >
      {loading ? <Loader2 size={16} className="spinning" aria-hidden="true" /> : icon}
      {children}
    </button>
  )
}

interface IconButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  label: string
  tone?: 'default' | 'danger'
  loading?: boolean
}

export function IconButton({
  label,
  tone = 'default',
  loading = false,
  children,
  className,
  disabled,
  ...rest
}: IconButtonProps) {
  return (
    <button
      {...rest}
      type="button"
      className={cx('ui-icon-button', tone === 'danger' && 'ui-danger-quiet', className)}
      aria-label={label}
      title={label}
      disabled={disabled || loading}
      aria-busy={loading || undefined}
    >
      {loading ? <Loader2 size={16} className="spinning" aria-hidden="true" /> : children}
    </button>
  )
}

// ---------------------------------------------------------------------------
// Surfaces
// ---------------------------------------------------------------------------

export function Card({
  children,
  className,
  as: Tag = 'section',
}: {
  children: ReactNode
  className?: string
  as?: 'section' | 'article' | 'div' | 'aside'
}) {
  return <Tag className={cx('ui-card', className)}>{children}</Tag>
}

type Tone = 'neutral' | 'ok' | 'warn' | 'bad' | 'info'

export function Badge({ tone = 'neutral', children }: { tone?: Tone; children: ReactNode }) {
  return (
    <span className={cx('ui-badge', `ui-badge-${tone}`)}>
      <span className="ui-badge-dot" aria-hidden="true" />
      {children}
    </span>
  )
}

export function Alert({
  tone = 'info',
  title,
  children,
  action,
}: {
  tone?: 'info' | 'success' | 'warning' | 'danger'
  title?: string
  children?: ReactNode
  action?: ReactNode
}) {
  const icon =
    tone === 'danger' || tone === 'warning' ? (
      <AlertCircle size={18} aria-hidden="true" />
    ) : tone === 'success' ? (
      <CheckCircle2 size={18} aria-hidden="true" />
    ) : (
      <Info size={18} aria-hidden="true" />
    )

  return (
    <div className={cx('ui-alert', `ui-alert-${tone}`)} role={tone === 'danger' ? 'alert' : 'status'}>
      <span className="ui-alert-icon">{icon}</span>
      <div className="ui-alert-body">
        {title && <strong>{title}</strong>}
        {children && <div className="ui-alert-text">{children}</div>}
        {action && <div className="ui-alert-action">{action}</div>}
      </div>
    </div>
  )
}

export function EmptyState({
  icon,
  title,
  children,
  action,
}: {
  icon?: ReactNode
  title: string
  children?: ReactNode
  action?: ReactNode
}) {
  return (
    <div className="ui-empty">
      {icon && <div className="ui-empty-icon">{icon}</div>}
      <strong>{title}</strong>
      {children && <p>{children}</p>}
      {action}
    </div>
  )
}

export function Skeleton({ lines = 3 }: { lines?: number }) {
  return (
    <div className="ui-skeleton" aria-hidden="true">
      {Array.from({ length: lines }, (_, index) => (
        <span key={index} />
      ))}
    </div>
  )
}

export function Spinner({ label }: { label: string }) {
  return (
    <span className="ui-inline-status" role="status">
      <Loader2 size={16} className="spinning" aria-hidden="true" />
      {label}
    </span>
  )
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

export function Field({
  label,
  htmlFor,
  hint,
  children,
}: {
  label: string
  htmlFor: string
  hint?: string
  children: ReactNode
}) {
  return (
    <div className="ui-field">
      <label htmlFor={htmlFor}>{label}</label>
      {children}
      {hint && <small>{hint}</small>}
    </div>
  )
}

export function TextInput(props: React.InputHTMLAttributes<HTMLInputElement>) {
  return <input {...props} className={cx('ui-input', props.className)} />
}

export function Select(props: React.SelectHTMLAttributes<HTMLSelectElement>) {
  return <select {...props} className={cx('ui-select', props.className)} />
}

// ---------------------------------------------------------------------------
// Table and definitions
// ---------------------------------------------------------------------------

export function Table({ children, label }: { children: ReactNode; label?: string }) {
  return (
    <div className="ui-table-wrap" tabIndex={0} role="region" aria-label={label}>
      <table className="ui-table">{children}</table>
    </div>
  )
}

export function DefinitionList({ rows }: { rows: Array<{ term: string; value: ReactNode }> }) {
  return (
    <dl className="ui-definitions">
      {rows.map(row => (
        <div key={row.term} className="ui-definition">
          <dt>{row.term}</dt>
          <dd>{row.value}</dd>
        </div>
      ))}
    </dl>
  )
}

// ---------------------------------------------------------------------------
// Copy and tooltip
// ---------------------------------------------------------------------------

async function copyText(value: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(value)

    return true
  } catch {
    try {
      const area = document.createElement('textarea')

      area.value = value
      area.setAttribute('readonly', '')
      area.style.position = 'fixed'
      area.style.opacity = '0'
      document.body.append(area)
      area.select()
      const ok = document.execCommand('copy')
      area.remove()

      return ok
    } catch {
      return false
    }
  }
}

export function CopyButton({
  value,
  label,
  compact = true,
}: {
  value: string
  label?: string
  compact?: boolean
}) {
  const { t } = useI18n()
  const [copied, setCopied] = useState(false)
  const timeout = useRef<number | null>(null)

  useEffect(
    () => () => {
      if (timeout.current !== null) {
        window.clearTimeout(timeout.current)
      }
    },
    [],
  )

  const copy = async () => {
    const ok = await copyText(value)

    if (!ok) {
      return
    }

    setCopied(true)
    timeout.current = window.setTimeout(() => setCopied(false), 1600)
  }

  const name = label ?? t('common.copyValue', { value: value.slice(0, 24) })

  return (
    <button
      type="button"
      className={cx('ui-copy', compact && 'ui-copy-compact')}
      onClick={() => void copy()}
      aria-label={name}
      title={copied ? t('common.copied') : name}
    >
      {copied ? <Check size={14} aria-hidden="true" /> : <Copy size={14} aria-hidden="true" />}
      {!compact && <span>{copied ? t('common.copied') : t('common.copy')}</span>}
    </button>
  )
}

export function Tooltip({ label, children }: { label: string; children: ReactNode }) {
  return (
    <span className="ui-tooltip" data-tooltip={label}>
      {children}
    </span>
  )
}

// ---------------------------------------------------------------------------
// Dialog
// ---------------------------------------------------------------------------

export function Dialog({
  open,
  title,
  children,
  footer,
  onClose,
  busy = false,
  size = 'regular',
}: {
  open: boolean
  title: string
  children: ReactNode
  footer?: ReactNode
  onClose: () => void
  busy?: boolean
  size?: 'small' | 'regular' | 'wide'
}) {
  const { t } = useI18n()
  const ref = useRef<HTMLDialogElement>(null)
  const titleId = useId()

  useEffect(() => {
    const dialog = ref.current

    if (!dialog) {
      return
    }

    if (open && !dialog.open) {
      dialog.showModal()
    } else if (!open && dialog.open) {
      dialog.close()
    }
  }, [open])

  useEffect(() => {
    const dialog = ref.current

    if (!dialog) {
      return
    }

    const cancel = (event: Event) => {
      if (busy) {
        event.preventDefault()

        return
      }

      onClose()
    }

    dialog.addEventListener('cancel', cancel)

    return () => dialog.removeEventListener('cancel', cancel)
  }, [busy, onClose])

  if (!open) {
    return null
  }

  return (
    <dialog
      ref={ref}
      className={cx('ui-dialog', `ui-dialog-${size}`)}
      aria-labelledby={titleId}
      onClick={event => {
        if (event.target === ref.current && !busy) {
          onClose()
        }
      }}
      onClose={() => {
        if (!busy) {
          onClose()
        }
      }}
    >
      <div className="ui-dialog-panel" onClick={event => event.stopPropagation()}>
        <header className="ui-dialog-header">
          <h2 id={titleId}>{title}</h2>
          <IconButton label={t('common.close')} onClick={onClose} disabled={busy}>
            <X size={18} aria-hidden="true" />
          </IconButton>
        </header>
        <div className="ui-dialog-body">{children}</div>
        {footer && <footer className="ui-dialog-footer">{footer}</footer>}
      </div>
    </dialog>
  )
}

export function ConfirmDialog({
  open,
  title,
  body,
  confirmLabel,
  cancelLabel,
  destructive = false,
  busy = false,
  onConfirm,
  onCancel,
}: {
  open: boolean
  title: string
  body: ReactNode
  confirmLabel: string
  cancelLabel?: string
  destructive?: boolean
  busy?: boolean
  onConfirm: () => void
  onCancel: () => void
}) {
  const { t } = useI18n()

  return (
    <Dialog
      open={open}
      title={title}
      onClose={onCancel}
      busy={busy}
      size="small"
      footer={
        <>
          <Button onClick={onCancel} disabled={busy}>
            {cancelLabel ?? t('common.cancel')}
          </Button>
          <Button
            variant={destructive ? 'danger' : 'primary'}
            loading={busy}
            onClick={onConfirm}
          >
            {confirmLabel}
          </Button>
        </>
      }
    >
      <p>{body}</p>
    </Dialog>
  )
}

// ---------------------------------------------------------------------------
// Toasts
// ---------------------------------------------------------------------------

interface ToastItem {
  id: string
  message: string
  tone: 'success' | 'danger' | 'info'
}

const ToastContext = createContext<{ notify: (message: string, tone?: ToastItem['tone']) => void } | null>(
  null,
)

export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<ToastItem[]>([])

  const notify = useCallback((message: string, tone: ToastItem['tone'] = 'success') => {
    const id =
      typeof crypto !== 'undefined' && 'randomUUID' in crypto
        ? crypto.randomUUID()
        : `${Date.now()}-${Math.random()}`

    setItems(current => [...current.slice(-2), { id, message, tone }])
    window.setTimeout(() => {
      setItems(current => current.filter(item => item.id !== id))
    }, 6000)
  }, [])

  return (
    <ToastContext.Provider value={{ notify }}>
      {children}
      <div className="ui-toasts" role="status" aria-live="polite">
        {items.map(item => (
          <div key={item.id} className={cx('ui-toast', `ui-toast-${item.tone}`)}>
            {item.message}
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  )
}

export function useToast() {
  const value = useContext(ToastContext)

  if (!value) {
    return { notify: () => undefined }
  }

  return value
}
