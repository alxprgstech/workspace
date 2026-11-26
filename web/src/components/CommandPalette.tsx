import { useEffect, useMemo, useRef, useState } from 'react'
import {
  Database,
  File as FileIcon,
  Folder,
  Languages,
  LayoutDashboard,
  Monitor,
  Moon,
  RefreshCw,
  Search,
  Settings,
  Sun,
  Terminal,
} from 'lucide-react'
import type { LucideIcon } from 'lucide-react'
import { useI18n } from '../i18n/provider.tsx'
import type { Locale, MessageKey } from '../i18n/index.ts'
import type { PageId } from '../router/core.ts'
import type { ThemeController } from '../theme/useTheme.ts'
import type { ThemePreference } from '../theme/core.ts'
import { cx, Dialog } from './ui.tsx'

type Group = 'palette.navigation' | 'palette.appearance' | 'palette.language' | 'palette.actions'

interface Command {
  id: string
  group: Group
  label: string
  keywords: string
  icon: LucideIcon
  run: () => void
}

const PAGE_ICONS: Record<PageId, LucideIcon> = {
  dashboard: LayoutDashboard,
  workspaces: Folder,
  jobs: Terminal,
  files: FileIcon,
  artifacts: Database,
  settings: Settings,
}

const PAGE_KEYS: Array<{ page: PageId; key: MessageKey }> = [
  { page: 'dashboard', key: 'nav.dashboard' },
  { page: 'workspaces', key: 'nav.workspaces' },
  { page: 'jobs', key: 'nav.jobs' },
  { page: 'files', key: 'nav.files' },
  { page: 'artifacts', key: 'nav.artifacts' },
  { page: 'settings', key: 'nav.settings' },
]

export function CommandPalette({
  open,
  onClose,
  navigate,
  theme,
  onRefresh,
}: {
  open: boolean
  onClose: () => void
  navigate: (page: PageId) => void
  theme: ThemeController
  onRefresh: () => void
}) {
  const { t, setLocale } = useI18n()
  const [query, setQuery] = useState('')
  const [active, setActive] = useState(0)
  const itemRefs = useRef<Array<HTMLButtonElement | null>>([])

  useEffect(() => {
    if (open) {
      setQuery('')
      setActive(0)
    }
  }, [open])

  const commands = useMemo<Command[]>(() => {
    const navigation = PAGE_KEYS.map(entry => ({
      id: `nav:${entry.page}`,
      group: 'palette.navigation' as Group,
      label: t(entry.key),
      keywords: `${entry.page} ${entry.key}`,
      icon: PAGE_ICONS[entry.page],
      run: () => navigate(entry.page),
    }))

    const themes: Array<{ preference: ThemePreference; key: MessageKey; icon: LucideIcon }> = [
      { preference: 'dark', key: 'palette.themeDark', icon: Moon },
      { preference: 'light', key: 'palette.themeLight', icon: Sun },
      { preference: 'system', key: 'palette.themeSystem', icon: Monitor },
    ]

    const appearance = themes.map(themeEntry => ({
      id: `theme:${themeEntry.preference}`,
      group: 'palette.appearance' as Group,
      label: t(themeEntry.key),
      keywords: `theme ${themeEntry.preference}`,
      icon: themeEntry.icon,
      run: () => theme.setPreference(themeEntry.preference),
    }))

    const languages: Array<{ value: Locale; key: MessageKey }> = [
      { value: 'ru', key: 'palette.languageRu' },
      { value: 'en', key: 'palette.languageEn' },
    ]

    const languageCommands = languages.map(entry => ({
      id: `lang:${entry.value}`,
      group: 'palette.language' as Group,
      label: t(entry.key),
      keywords: `language ${entry.value} ${entry.value === 'ru' ? 'русский' : 'english'}`,
      icon: Languages,
      run: () => setLocale(entry.value),
    }))

    const actions: Command[] = [
      {
        id: 'action:refresh',
        group: 'palette.actions',
        label: t('palette.refresh'),
        keywords: 'refresh reload обновить',
        icon: RefreshCw,
        run: onRefresh,
      },
    ]

    return [...navigation, ...appearance, ...languageCommands, ...actions]
  }, [navigate, onRefresh, setLocale, t, theme])

  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase()

    if (!needle) {
      return commands
    }

    return commands.filter(command =>
      `${command.label} ${command.keywords}`.toLowerCase().includes(needle),
    )
  }, [commands, query])

  useEffect(() => {
    setActive(current => (filtered.length === 0 ? 0 : Math.min(current, filtered.length - 1)))
  }, [filtered])

  useEffect(() => {
    itemRefs.current[active]?.scrollIntoView({ block: 'nearest' })
  }, [active])

  const run = (command: Command) => {
    onClose()
    window.setTimeout(command.run, 0)
  }

  const groups = useMemo(() => {
    const order: Group[] = [
      'palette.navigation',
      'palette.appearance',
      'palette.language',
      'palette.actions',
    ]

    return order
      .map(group => ({
        group,
        items: filtered
          .map((command, index) => ({ command, index }))
          .filter(entry => entry.command.group === group),
      }))
      .filter(section => section.items.length > 0)
  }, [filtered])

  return (
    <Dialog
      open={open}
      title={t('palette.title')}
      onClose={onClose}
      size="regular"
      footer={<p className="palette-hint">{t('palette.hint')}</p>}
    >
      <div className="palette-search">
        <Search size={18} aria-hidden="true" />
        <input
          autoFocus
          type="text"
          role="combobox"
          aria-expanded="true"
          aria-controls="palette-list"
          aria-activedescendant={filtered[active] ? `palette-${filtered[active].id}` : undefined}
          aria-label={t('palette.placeholder')}
          placeholder={t('palette.placeholder')}
          value={query}
          onChange={event => setQuery(event.target.value)}
          onKeyDown={event => {
            if (event.key === 'ArrowDown') {
              event.preventDefault()
              setActive(current => (filtered.length === 0 ? 0 : (current + 1) % filtered.length))
            } else if (event.key === 'ArrowUp') {
              event.preventDefault()
              setActive(current =>
                filtered.length === 0 ? 0 : (current - 1 + filtered.length) % filtered.length,
              )
            } else if (event.key === 'Enter') {
              event.preventDefault()
              const command = filtered[active]

              if (command) {
                run(command)
              }
            }
          }}
        />
      </div>

      <div className="palette-list" id="palette-list" role="listbox" aria-label={t('palette.title')}>
        {filtered.length === 0 && <p className="palette-empty">{t('palette.empty')}</p>}
        {groups.map(section => (
          <div key={section.group} className="palette-group" role="group" aria-label={t(section.group)}>
            <p className="palette-group-title">{t(section.group)}</p>
            {section.items.map(({ command, index }) => {
              const Icon = command.icon

              return (
                <button
                  key={command.id}
                  id={`palette-${command.id}`}
                  type="button"
                  role="option"
                  aria-selected={index === active}
                  ref={element => {
                    itemRefs.current[index] = element
                  }}
                  className={cx('palette-item', index === active && 'is-active')}
                  onMouseEnter={() => setActive(index)}
                  onClick={() => run(command)}
                >
                  <Icon size={16} aria-hidden="true" />
                  <span>{command.label}</span>
                </button>
              )
            })}
          </div>
        ))}
      </div>
    </Dialog>
  )
}
