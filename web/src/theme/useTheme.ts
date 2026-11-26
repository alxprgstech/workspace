import { useCallback, useEffect, useState } from 'react'
import {
  readThemePreference,
  resolveTheme,
  THEME_KEY,
  writeThemePreference,
  type ResolvedTheme,
  type ThemePreference,
} from './core.ts'

function storage(): Storage | null {
  try {
    return window.localStorage
  } catch {
    return null
  }
}

export interface ThemeController {
  preference: ThemePreference
  resolved: ResolvedTheme
  setPreference: (preference: ThemePreference) => void
}

export function useTheme(): ThemeController {
  const [preference, setPreferenceState] = useState<ThemePreference>(() =>
    readThemePreference(storage()),
  )
  const [systemDark, setSystemDark] = useState<boolean>(
    () => window.matchMedia('(prefers-color-scheme: dark)').matches,
  )

  useEffect(() => {
    const media = window.matchMedia('(prefers-color-scheme: dark)')

    const update = () => setSystemDark(media.matches)
    const sync = (event: StorageEvent) => {
      if (event.key === THEME_KEY || event.key === null) {
        setPreferenceState(readThemePreference(storage()))
      }
    }

    media.addEventListener('change', update)
    window.addEventListener('storage', sync)

    return () => {
      media.removeEventListener('change', update)
      window.removeEventListener('storage', sync)
    }
  }, [])

  const resolved = resolveTheme(preference, systemDark)

  useEffect(() => {
    const root = document.documentElement

    root.dataset.theme = resolved
    root.dataset.themePreference = preference
    root.style.colorScheme = resolved

    document
      .querySelector('meta[name="theme-color"]')
      ?.setAttribute('content', resolved === 'dark' ? '#090a0c' : '#f7f8fa')
  }, [preference, resolved])

  const setPreference = useCallback((next: ThemePreference) => {
    writeThemePreference(storage(), next)
    setPreferenceState(next)
  }, [])

  return { preference, resolved, setPreference }
}
