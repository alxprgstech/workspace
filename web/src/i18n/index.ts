import { en, type MessageKey, type Messages } from './en.ts'
import { ru } from './ru.ts'

export type { MessageKey, Messages }
export { en, ru }

export type Locale = 'ru' | 'en'

/** Browser-only, non-secret preference. */
export const LANGUAGE_KEY = 'alxprgs.workspace.lang.v1'

export const LOCALES: readonly Locale[] = ['ru', 'en'] as const

export const dictionaries: Record<Locale, Messages> = { ru, en }

export function isLocale(value: unknown): value is Locale {
  return value === 'ru' || value === 'en'
}

/**
 * Explicit choice wins; otherwise Russian for a Russian browser, English for
 * everything else. This mirrors the SSO locale policy without pretending that
 * the browser speaks for the user forever.
 */
export function detectLocale(
  stored: string | null | undefined,
  languages: readonly string[],
  fallback: Locale = 'ru',
): Locale {
  if (isLocale(stored)) {
    return stored
  }

  for (const language of languages) {
    const primary = language.toLowerCase().split('-')[0]

    if (isLocale(primary)) {
      return primary
    }
  }

  return fallback
}

/** Interpolate `{name}` placeholders without ever interpreting the result as HTML. */
export function translate(
  messages: Messages,
  key: MessageKey,
  variables?: Readonly<Record<string, string | number>>,
): string {
  const template = messages[key]

  if (!variables) {
    return template
  }

  return template.replace(/\{(\w+)\}/g, (match, name: string) => {
    const value = variables[name]

    return value === undefined ? match : String(value)
  })
}

export interface PluralForms {
  one: string
  few?: string
  many?: string
  other: string
}

/** Locale-aware plural selection for counts shown in the interface. */
export function plural(locale: Locale, count: number, forms: PluralForms): string {
  const category = new Intl.PluralRules(locale).select(count)

  switch (category) {
    case 'one':
      return forms.one
    case 'few':
      return forms.few ?? forms.other
    case 'many':
      return forms.many ?? forms.other
    default:
      return forms.other
  }
}

export interface Formatters {
  date: (unixSeconds: number | null | undefined) => string
  dateTime: (unixSeconds: number | null | undefined) => string
  number: (value: number) => string
  bytes: (value: number) => string
  duration: (seconds: number | null | undefined) => string
}

const BYTE_UNITS: Record<Locale, readonly string[]> = {
  ru: ['Б', 'КиБ', 'МиБ', 'ГиБ', 'ТиБ'],
  en: ['B', 'KiB', 'MiB', 'GiB', 'TiB'],
}

export function createFormatters(locale: Locale): Formatters {
  const date = new Intl.DateTimeFormat(locale, { dateStyle: 'medium' })
  const dateTime = new Intl.DateTimeFormat(locale, {
    dateStyle: 'medium',
    timeStyle: 'short',
  })
  const number = new Intl.NumberFormat(locale)

  const formatUnix = (formatter: Intl.DateTimeFormat) =>
    (unixSeconds: number | null | undefined): string => {
      if (unixSeconds === null || unixSeconds === undefined || unixSeconds <= 0) {
        return '—'
      }

      return formatter.format(new Date(unixSeconds * 1000))
    }

  return {
    date: formatUnix(date),
    dateTime: formatUnix(dateTime),
    number: value => number.format(value),
    bytes: value => {
      if (!Number.isFinite(value) || value < 0) {
        return '—'
      }

      let scaled = value
      let unit = 0

      while (scaled >= 1024 && unit < BYTE_UNITS[locale].length - 1) {
        scaled /= 1024
        unit += 1
      }

      const digits = unit === 0 ? 0 : scaled < 10 ? 1 : 0
      const formatted = new Intl.NumberFormat(locale, {
        minimumFractionDigits: digits,
        maximumFractionDigits: digits,
      }).format(scaled)

      return `${formatted} ${BYTE_UNITS[locale][unit]}`
    },
    duration: seconds => {
      if (seconds === null || seconds === undefined || seconds < 0) {
        return '—'
      }

      const total = Math.floor(seconds)
      const hours = Math.floor(total / 3600)
      const minutes = Math.floor((total % 3600) / 60)
      const rest = total % 60

      if (hours > 0) {
        return `${number.format(hours)} ${locale === 'ru' ? 'ч' : 'h'} ${number.format(minutes)} ${
          locale === 'ru' ? 'мин' : 'min'
        }`
      }

      if (minutes > 0) {
        return `${number.format(minutes)} ${locale === 'ru' ? 'мин' : 'min'} ${number.format(rest)} ${
          locale === 'ru' ? 'с' : 's'
        }`
      }

      return `${number.format(rest)} ${locale === 'ru' ? 'с' : 's'}`
    },
  }
}

/** Localized labels for stable API state codes; raw codes stay available. */
export const WORKSPACE_STATES: Record<string, Record<Locale, string>> = {
  created: { ru: 'Создано', en: 'Created' },
  preparing: { ru: 'Подготовка', en: 'Preparing' },
  starting: { ru: 'Запуск', en: 'Starting' },
  running: { ru: 'Выполняется', en: 'Running' },
  idle: { ru: 'Простаивает', en: 'Idle' },
  stopping: { ru: 'Остановка', en: 'Stopping' },
  stopped: { ru: 'Остановлено', en: 'Stopped' },
  failed: { ru: 'Ошибка', en: 'Failed' },
  deleting: { ru: 'Удаление', en: 'Deleting' },
  expired: { ru: 'Истекло', en: 'Expired' },
}

export const JOB_STATES: Record<string, Record<Locale, string>> = {
  queued: { ru: 'В очереди', en: 'Queued' },
  running: { ru: 'Выполняется', en: 'Running' },
  succeeded: { ru: 'Успешно', en: 'Succeeded' },
  failed: { ru: 'Ошибка', en: 'Failed' },
  cancelled: { ru: 'Отменено', en: 'Cancelled' },
  timed_out: { ru: 'Превышен лимит времени', en: 'Timed out' },
  interrupted: { ru: 'Прервано', en: 'Interrupted' },
}

export const SERVICE_STATES: Record<string, Record<Locale, string>> = {
  ready: { ru: 'Готов', en: 'Ready' },
  partial: { ru: 'Частично', en: 'Partial' },
  not_ready: { ru: 'Не готов', en: 'Not ready' },
  available: { ru: 'Доступен', en: 'Available' },
  not_installed: { ru: 'Не установлен', en: 'Not installed' },
  degraded: { ru: 'Ограниченно', en: 'Degraded' },
}

/** Journal status of an update or rollback. */
export const UPDATE_STATES: Record<string, Record<Locale, string>> = {
  prepared: { ru: 'Подготовлено', en: 'Prepared' },
  running: { ru: 'Выполняется', en: 'Running' },
  succeeded: { ru: 'Успешно', en: 'Succeeded' },
  failed: { ru: 'Ошибка', en: 'Failed' },
  rolled_back: { ru: 'Откат выполнен', en: 'Rolled back' },
  rollback_failed: { ru: 'Откат не удался', en: 'Rollback failed' },
  interrupted: { ru: 'Прервано', en: 'Interrupted' },
}

/** Steps of an update or rollback. */
export const UPDATE_STAGES: Record<string, Record<Locale, string>> = {
  prepared: { ru: 'Подготовка', en: 'Preparing' },
  drain: { ru: 'Ожидание заданий', en: 'Waiting for active jobs' },
  stop: { ru: 'Остановка сервера', en: 'Stopping the server' },
  backup: { ru: 'Резервное копирование', en: 'Backing up' },
  install_runtime: { ru: 'Обновление компонентов WSL', en: 'Installing WSL components' },
  migrate: { ru: 'Миграция базы данных', en: 'Migrating the database' },
  install_windows: { ru: 'Установка Windows-бинарника', en: 'Installing the Windows binary' },
  start: { ru: 'Запуск сервера', en: 'Starting the server' },
  verify: { ru: 'Проверка работоспособности', en: 'Verifying the installation' },
  rollback: { ru: 'Откат', en: 'Rolling back' },
  safety_copy: { ru: 'Сохранение текущей базы', en: 'Saving the current database' },
  restore_database: { ru: 'Восстановление базы', en: 'Restoring the database' },
  restore_windows: { ru: 'Восстановление бинарника', en: 'Restoring the Windows binary' },
  restore_runtime: { ru: 'Восстановление WSL', en: 'Restoring WSL components' },
  succeeded: { ru: 'Завершено', en: 'Finished' },
  failed: { ru: 'Ошибка', en: 'Failed' },
  rolled_back: { ru: 'Откат выполнен', en: 'Rolled back' },
  rollback_failed: { ru: 'Откат не удался', en: 'Rollback failed' },
  interrupted: { ru: 'Прервано', en: 'Interrupted' },
}

export function stateLabel(
  table: Record<string, Record<Locale, string>>,
  code: string,
  locale: Locale,
): string {
  return table[code]?.[locale] ?? code
}
