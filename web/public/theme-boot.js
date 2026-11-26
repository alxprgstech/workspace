/*
 * Runs before first paint: applies the stored theme and language so the page
 * never flashes the wrong appearance. Mirrors ALXPRGS SSO `public/theme/theme.js`,
 * with the Workspace keys and the legacy `workspace-theme` migration.
 * No network, no external resources, no secrets.
 */
(function () {
  var THEME_KEY = 'alxprgs.workspace.theme.v1'
  var LEGACY_THEME_KEY = 'workspace-theme'
  var LANG_KEY = 'alxprgs.workspace.lang.v1'

  function read(key) {
    try {
      return window.localStorage.getItem(key)
    } catch (error) {
      return null
    }
  }

  var preference = 'system'
  var storedTheme = read(THEME_KEY)

  if (storedTheme === 'dark' || storedTheme === 'light' || storedTheme === 'system') {
    preference = storedTheme
  } else {
    var legacy = read(LEGACY_THEME_KEY)

    if (legacy === 'dark' || legacy === 'light') {
      preference = legacy
    }
  }

  var systemDark = window.matchMedia
    ? window.matchMedia('(prefers-color-scheme: dark)').matches
    : true
  var resolved =
    preference === 'system' ? (systemDark ? 'dark' : 'light') : preference

  var root = document.documentElement

  root.dataset.theme = resolved
  root.dataset.themePreference = preference
  root.style.colorScheme = resolved

  var storedLanguage = read(LANG_KEY)
  var language = 'ru'

  if (storedLanguage === 'ru' || storedLanguage === 'en') {
    language = storedLanguage
  } else {
    var languages = navigator.languages || [navigator.language || '']

    for (var index = 0; index < languages.length; index += 1) {
      var primary = String(languages[index] || '').toLowerCase().split('-')[0]

      if (primary === 'ru' || primary === 'en') {
        language = primary
        break
      }
    }
  }

  root.lang = language

  var meta = document.querySelector('meta[name="theme-color"]')

  if (meta) {
    meta.setAttribute('content', resolved === 'dark' ? '#090a0c' : '#f7f8fa')
  }
})()
