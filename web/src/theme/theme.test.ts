import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  isThemePreference,
  LEGACY_THEME_KEY,
  readThemePreference,
  resolveTheme,
  THEME_KEY,
  writeThemePreference,
} from './core.ts'

function storage(values: Record<string, string> = {}) {
  const data = new Map(Object.entries(values))

  return {
    getItem: (key: string) => data.get(key) ?? null,
    setItem: (key: string, value: string) => {
      data.set(key, value)
    },
    dump: () => Object.fromEntries(data),
  }
}

test('theme preference validation accepts only the three modes', () => {
  assert.ok(isThemePreference('dark'))
  assert.ok(isThemePreference('light'))
  assert.ok(isThemePreference('system'))
  assert.ok(!isThemePreference('blue'))
  assert.ok(!isThemePreference(null))
})

test('stored preference wins and legacy boolean migrates', () => {
  assert.equal(readThemePreference(storage({ [THEME_KEY]: 'light' })), 'light')
  assert.equal(readThemePreference(storage({ [LEGACY_THEME_KEY]: 'dark' })), 'dark')
  assert.equal(readThemePreference(storage({ [LEGACY_THEME_KEY]: 'light' })), 'light')
  assert.equal(readThemePreference(storage()), 'system')
  assert.equal(readThemePreference(null), 'system')
})

test('invalid stored values fall back to the system preference', () => {
  assert.equal(readThemePreference(storage({ [THEME_KEY]: 'rainbow' })), 'system')
})

test('resolution follows the system only in system mode', () => {
  assert.equal(resolveTheme('system', true), 'dark')
  assert.equal(resolveTheme('system', false), 'light')
  assert.equal(resolveTheme('dark', false), 'dark')
  assert.equal(resolveTheme('light', true), 'light')
})

test('writing persists the new key and mirrors the legacy key', () => {
  const store = storage()
  writeThemePreference(store, 'dark')
  assert.equal(store.dump()[THEME_KEY], 'dark')
  assert.equal(store.dump()[LEGACY_THEME_KEY], 'dark')

  writeThemePreference(store, 'system')
  assert.equal(store.dump()[THEME_KEY], 'system')
})
