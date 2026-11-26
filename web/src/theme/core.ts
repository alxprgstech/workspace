/**
 * Theme preference: pure functions, no DOM, so they can be unit-tested in Node.
 *
 * The key and semantics match ALXPRGS SSO (`alxprgs.ui.theme.v1` there), with a
 * Workspace-specific key so the two applications do not overwrite each other.
 */
export type ThemePreference = 'dark' | 'light' | 'system'
export type ResolvedTheme = 'dark' | 'light'

export const THEME_KEY = 'alxprgs.workspace.theme.v1'
/** The previous Workspace stored a boolean theme under this key. */
export const LEGACY_THEME_KEY = 'workspace-theme'

export function isThemePreference(value: unknown): value is ThemePreference {
  return value === 'dark' || value === 'light' || value === 'system'
}

/**
 * Read the stored preference, migrating the legacy `workspace-theme` key once.
 * Returns `system` when nothing valid is stored.
 */
export function readThemePreference(storage: Pick<Storage, 'getItem'> | null): ThemePreference {
  if (!storage) {
    return 'system'
  }

  try {
    const current = storage.getItem(THEME_KEY)

    if (isThemePreference(current)) {
      return current
    }

    const legacy = storage.getItem(LEGACY_THEME_KEY)

    if (legacy === 'dark' || legacy === 'light') {
      return legacy
    }
  } catch {
    // Storage may be blocked; fall back to the system preference.
  }

  return 'system'
}

export function resolveTheme(preference: ThemePreference, systemDark: boolean): ResolvedTheme {
  if (preference === 'system') {
    return systemDark ? 'dark' : 'light'
  }

  return preference
}

export function writeThemePreference(
  storage: Pick<Storage, 'setItem'> | null,
  preference: ThemePreference,
): void {
  if (!storage) {
    return
  }

  try {
    storage.setItem(THEME_KEY, preference)
    storage.setItem(LEGACY_THEME_KEY, preference === 'system' ? '' : preference)
  } catch {
    // Keep the choice in memory for this page.
  }
}
