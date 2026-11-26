import { useEffect, useRef, useState, type ReactNode } from 'react'
import {
  Boxes,
  ChevronDown,
  Command,
  Database,
  File as FileIcon,
  Folder,
  Globe2,
  LayoutDashboard,
  LogOut,
  Menu,
  Monitor,
  Moon,
  RefreshCw,
  Settings,
  Sun,
  Terminal,
  UserRound,
} from 'lucide-react'
import type { LucideIcon } from 'lucide-react'
import type { SessionInfo } from '../api.ts'
import { useI18n } from '../i18n/provider.tsx'
import type { MessageKey } from '../i18n/index.ts'
import type { PageId, Route } from '../router/core.ts'
import { cx, Badge, Button, Dialog, IconButton, Select } from './ui.tsx'
import { Brand } from './Brand.tsx'
import type { ThemeController } from '../theme/useTheme.ts'
import type { ThemePreference } from '../theme/core.ts'

interface NavEntry {
  page: PageId
  icon: LucideIcon
  key: MessageKey
}

const NAV: NavEntry[] = [
  { page: 'dashboard', icon: LayoutDashboard, key: 'nav.dashboard' },
  { page: 'workspaces', icon: Folder, key: 'nav.workspaces' },
  { page: 'jobs', icon: Terminal, key: 'nav.jobs' },
  { page: 'files', icon: FileIcon, key: 'nav.files' },
  { page: 'artifacts', icon: Database, key: 'nav.artifacts' },
  { page: 'settings', icon: Settings, key: 'nav.settings' },
  // Administrator-only: the server rejects the API for every other role.
  { page: 'updates', icon: RefreshCw, key: 'nav.updates' },
]

function Navigation({
  route,
  onNavigate,
  admin,
}: {
  route: Route
  onNavigate: (page: PageId) => void
  admin: boolean
}) {
  const { t } = useI18n()
  const list = useRef<HTMLElement>(null)
  const buttons = useRef(new Map<PageId, HTMLButtonElement>())
  const [indicator, setIndicator] = useState<{ top: number; height: number } | null>(null)

  useEffect(() => {
    const active = buttons.current.get(route.page)

    if (!active || !list.current) {
      setIndicator(null)

      return
    }

    const update = () => {
      setIndicator({ top: active.offsetTop, height: active.offsetHeight })
    }

    update()
    const observer = new ResizeObserver(update)
    observer.observe(list.current)
    window.addEventListener('resize', update)

    return () => {
      observer.disconnect()
      window.removeEventListener('resize', update)
    }
  }, [route.page])

  return (
    <nav className="app-nav" aria-label={t('nav.label')} ref={list}>
      {indicator && (
        <span
          className="nav-indicator"
          aria-hidden="true"
          style={{ transform: `translateY(${indicator.top}px)`, height: indicator.height }}
        />
      )}
      {NAV.filter(entry => admin || entry.page !== 'updates').map(entry => {
        const Icon = entry.icon
        const active = route.page === entry.page

        return (
          <button
            key={entry.page}
            type="button"
            ref={element => {
              if (element) {
                buttons.current.set(entry.page, element)
              } else {
                buttons.current.delete(entry.page)
              }
            }}
            className={cx('nav-item', active && 'is-active')}
            aria-current={active ? 'page' : undefined}
            onClick={() => onNavigate(entry.page)}
          >
            <Icon size={18} aria-hidden="true" />
            <span>{t(entry.key)}</span>
          </button>
        )
      })}
    </nav>
  )
}

const THEME_ICONS = { dark: Moon, light: Sun, system: Monitor } as const

function AppearanceControls({
  theme,
  compact = false,
}: {
  theme: ThemeController
  compact?: boolean
}) {
  const { t, locale, setLocale } = useI18n()
  const ThemeIcon = THEME_ICONS[theme.preference]

  return (
    <div className={cx('appearance-controls', compact && 'is-compact')}>
      <label className="control-select">
        <Globe2 size={16} aria-hidden="true" />
        <Select
          aria-label={t('settings.language')}
          value={locale}
          onChange={event => setLocale(event.target.value === 'ru' ? 'ru' : 'en')}
        >
          <option value="ru">RU</option>
          <option value="en">EN</option>
        </Select>
      </label>
      <label className="control-select">
        <ThemeIcon size={16} aria-hidden="true" />
        <Select
          aria-label={t('settings.theme')}
          value={theme.preference}
          onChange={event => theme.setPreference(event.target.value as ThemePreference)}
        >
          <option value="dark">{t('settings.themeDark')}</option>
          <option value="light">{t('settings.themeLight')}</option>
          <option value="system">{t('settings.themeSystem')}</option>
        </Select>
      </label>
    </div>
  )
}

export interface AppLayoutProps {
  route: Route
  navigate: (page: PageId) => void
  theme: ThemeController
  session: SessionInfo
  version: string
  signingOut: boolean
  onSignOut: () => void
  onOpenCommands: () => void
  children: ReactNode
}

export function AppLayout({
  route,
  navigate,
  theme,
  session,
  version,
  signingOut,
  onSignOut,
  onOpenCommands,
  children,
}: AppLayoutProps) {
  const { t } = useI18n()
  const [sectionsOpen, setSectionsOpen] = useState(false)
  const displayName = session.user.username ?? session.user.subject
  const role = session.user.role === 'admin' ? t('auth.roleAdmin') : t('auth.roleUser')
  const admin = session.user.role === 'admin'

  const go = (page: PageId) => {
    setSectionsOpen(false)
    navigate(page)
  }

  return (
    <div className="app-shell">
      <header className="app-topbar">
        <div className="app-topbar-start">
          <button
            type="button"
            className="ui-icon-button mobile-only"
            aria-label={t('nav.openSections')}
            onClick={() => setSectionsOpen(true)}
          >
            <Menu size={18} aria-hidden="true" />
          </button>
          <a
            className="app-brand-link"
            href="#/dashboard"
            onClick={event => {
              event.preventDefault()
              go('dashboard')
            }}
            aria-label={`${t('app.product')} ${t('app.name')}`}
          >
            <Brand />
          </a>
        </div>
        <div className="app-topbar-end">
          <div className="user-chip" title={session.user.issuer}>
            <span className="user-avatar" aria-hidden="true">
              <UserRound size={16} />
            </span>
            <span className="user-meta">
              <strong>{displayName}</strong>
              <Badge tone={session.user.role === 'admin' ? 'info' : 'neutral'}>{role}</Badge>
            </span>
          </div>
          <Button
            className="commands-button"
            compact
            icon={<Command size={15} aria-hidden="true" />}
            onClick={onOpenCommands}
            aria-label={t('palette.open')}
          >
            <span className="hide-on-mobile">{t('palette.title')}</span>
            <kbd className="hide-on-mobile">{t('palette.shortcut')}</kbd>
          </Button>
          <AppearanceControls theme={theme} compact />
          <IconButton
            label={t('auth.signOut')}
            loading={signingOut}
            onClick={onSignOut}
            tone="default"
          >
            <LogOut size={17} aria-hidden="true" />
          </IconButton>
        </div>
      </header>

      <div className="app-body">
        <aside className="app-sidebar">
          <p className="nav-heading">{t('nav.management')}</p>
          <Navigation route={route} onNavigate={go} admin={admin} />
          <div className="sidebar-foot">
            <Boxes size={15} aria-hidden="true" />
            <span>
              {t('app.footer')} · v{version}
            </span>
            <ChevronDown size={13} aria-hidden="true" />
          </div>
        </aside>

        <main className="app-main" id="main">
          {children}
        </main>
      </div>

      <Dialog
        open={sectionsOpen}
        title={t('nav.openSections')}
        onClose={() => setSectionsOpen(false)}
        size="small"
      >
        <Navigation route={route} onNavigate={go} admin={admin} />
        <div className="mobile-appearance">
          <AppearanceControls theme={theme} />
        </div>
      </Dialog>
    </div>
  )
}

export { AppearanceControls }
