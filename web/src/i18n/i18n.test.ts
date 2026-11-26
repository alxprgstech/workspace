import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  createFormatters,
  detectLocale,
  dictionaries,
  en,
  plural,
  ru,
  translate,
} from './index.ts'

test('ru and en dictionaries have identical key sets', () => {
  const english = Object.keys(en).sort()
  const russian = Object.keys(ru).sort()

  assert.deepEqual(russian, english)
})

test('no message is empty and interpolated placeholders are balanced', () => {
  for (const [locale, messages] of Object.entries(dictionaries)) {
    for (const [key, value] of Object.entries(messages)) {
      assert.ok(value.trim().length > 0, `${locale}:${key} is empty`)

      const opened = (value.match(/\{/g) ?? []).length
      const closed = (value.match(/\}/g) ?? []).length

      assert.equal(opened, closed, `${locale}:${key} has unbalanced braces`)
    }
  }
})

test('translation interpolates values and keeps unknown placeholders', () => {
  assert.equal(
    translate(en, 'common.usedOf', { used: '1 MiB', total: '2 MiB' }),
    '1 MiB of 2 MiB',
  )
  assert.equal(
    translate(ru, 'common.usedOf', { used: '1 МиБ', total: '2 МиБ' }),
    '1 МиБ из 2 МиБ',
  )
  assert.equal(translate(en, 'common.cores', {}), '{count} cores')
})

test('locale detection prefers the explicit choice, then the browser', () => {
  assert.equal(detectLocale('en', ['ru-RU']), 'en')
  assert.equal(detectLocale('ru', ['en-US']), 'ru')
  assert.equal(detectLocale(null, ['ru-RU', 'en-US']), 'ru')
  assert.equal(detectLocale(null, ['de-DE', 'en-GB']), 'en')
  assert.equal(detectLocale(undefined, []), 'ru')
  assert.equal(detectLocale('klingon', ['fr-FR'], 'en'), 'en')
})

test('plural selection follows the locale rules', () => {
  const forms = { one: 'one', few: 'few', many: 'many', other: 'other' }

  assert.equal(plural('en', 1, forms), 'one')
  assert.equal(plural('en', 2, forms), 'other')
  assert.equal(plural('ru', 1, forms), 'one')
  assert.equal(plural('ru', 2, forms), 'few')
  assert.equal(plural('ru', 5, forms), 'many')
  assert.equal(plural('ru', 1.5, forms), 'other')
})

test('formatters localise numbers, sizes, dates and durations', () => {
  const english = createFormatters('en')
  const russian = createFormatters('ru')

  assert.match(english.bytes(1536), /1\.5 KiB/)
  assert.match(russian.bytes(1536), /1,5 КиБ/)
  assert.equal(english.date(null), '—')
  assert.match(english.date(1_700_000_000), /2023/)
  assert.equal(english.duration(65), '1 min 5 s')
  assert.equal(russian.duration(65), '1 мин 5 с')
  assert.match(english.number(1234567), /1,234,567/)
})
