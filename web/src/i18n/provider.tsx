import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react'
import {
  createFormatters,
  detectLocale,
  dictionaries,
  LANGUAGE_KEY,
  translate,
  type Formatters,
  type Locale,
  type MessageKey,
} from './index.ts'

interface I18nValue {
  locale: Locale
  setLocale: (locale: Locale) => void
  t: (key: MessageKey, variables?: Readonly<Record<string, string | number>>) => string
  format: Formatters
}

const I18nContext = createContext<I18nValue | null>(null)

function initialLocale(): Locale {
  let stored: string | null = null

  try {
    stored = window.localStorage.getItem(LANGUAGE_KEY)
  } catch {
    // Storage may be blocked; the browser language still decides.
  }

  return detectLocale(stored, navigator.languages ?? [navigator.language])
}

export function I18nProvider({ children }: { children: ReactNode }) {
  const [locale, setLocaleState] = useState<Locale>(initialLocale)

  useEffect(() => {
    document.documentElement.lang = locale

    try {
      window.localStorage.setItem(LANGUAGE_KEY, locale)
    } catch {
      // Keep the choice in memory.
    }
  }, [locale])

  const setLocale = useCallback((next: Locale) => setLocaleState(next), [])

  const value = useMemo<I18nValue>(() => {
    const messages = dictionaries[locale]

    return {
      locale,
      setLocale,
      t: (key, variables) => translate(messages, key, variables),
      format: createFormatters(locale),
    }
  }, [locale, setLocale])

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>
}

export function useI18n(): I18nValue {
  const value = useContext(I18nContext)

  if (!value) {
    throw new Error('useI18n must be used inside I18nProvider')
  }

  return value
}
