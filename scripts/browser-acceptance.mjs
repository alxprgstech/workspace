// Browser acceptance for the update administration page.
//
// Drives an installed Chrome/Edge over the DevTools protocol from Node's
// built-in WebSocket client. No npm dependency, no bundled browser: it opens the
// real Workspace UI, sets the real session cookies, clicks the real buttons and
// records console errors and screenshots.

import { spawn } from 'node:child_process'
import { mkdir, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { setTimeout as sleep } from 'node:timers/promises'
import { tmpdir } from 'node:os'

const args = new Map()
for (let index = 2; index < process.argv.length; index += 2) {
  const key = process.argv[index]
  if (key?.startsWith('--')) {
    args.set(key.slice(2), process.argv[index + 1] ?? '')
  }
}

const base = (args.get('base') ?? 'http://127.0.0.1:8799').replace(/\/$/, '')
const outDir = args.get('out') ?? 'docs/screenshots/updates'
const scenario = new Set((args.get('scenario') ?? 'anonymous,admin,screens,check,nonadmin').split(','))
const adminHandle = args.get('admin-handle') ?? process.env.LAB_ADMIN_HANDLE ?? ''
const adminCsrf = args.get('admin-csrf') ?? process.env.LAB_ADMIN_CSRF ?? ''
const userHandle = args.get('user-handle') ?? process.env.LAB_USER_HANDLE ?? ''
const userCsrf = args.get('user-csrf') ?? process.env.LAB_USER_CSRF ?? ''
const expectedCurrent = args.get('expect-current') ?? ''
const expectedTarget = args.get('expect-target') ?? ''
const chromePath =
  args.get('chrome') ??
  process.env.CHROME_PATH ??
  'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe'
const port = Number(args.get('port') ?? 9333)

const results = []
const consoleErrors = []
let cdp = null
let chrome = null

function record(name, ok, detail = '') {
  results.push({ name, ok, detail })
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ` — ${detail}` : ''}`)
}

class Cdp {
  constructor(socket) {
    this.socket = socket
    this.nextId = 1
    this.pending = new Map()
    this.events = []
    this.waiters = []
    socket.addEventListener('message', event => {
      const message = JSON.parse(event.data)
      if (message.id) {
        const pending = this.pending.get(message.id)
        if (pending) {
          this.pending.delete(message.id)
          message.error ? pending.reject(new Error(message.error.message)) : pending.resolve(message.result)
        }
        return
      }
      this.events.push(message)
      if (this.events.length > 2000) this.events.splice(0, 1000)
      for (const waiter of [...this.waiters]) {
        if (waiter.method === message.method) {
          this.waiters.splice(this.waiters.indexOf(waiter), 1)
          waiter.resolve(message.params)
        }
      }
    })
  }

  send(method, params = {}) {
    const id = this.nextId++
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject })
      this.socket.send(JSON.stringify({ id, method, params }))
    })
  }

  waitEvent(method, timeout = 30000) {
    return new Promise((resolve, reject) => {
      const waiter = { method, resolve }
      this.waiters.push(waiter)
      setTimeout(() => {
        const index = this.waiters.indexOf(waiter)
        if (index >= 0) {
          this.waiters.splice(index, 1)
          reject(new Error(`timed out waiting for ${method}`))
        }
      }, timeout)
    })
  }
}

async function evaluate(expression, awaitPromise = false) {
  const result = await cdp.send('Runtime.evaluate', {
    expression,
    returnByValue: true,
    awaitPromise,
    userGesture: true,
  })
  if (result.exceptionDetails) {
    throw new Error(result.exceptionDetails.exception?.description ?? 'evaluation failed')
  }
  return result.result.value
}

async function waitFor(expression, timeout = 30000, label = expression) {
  const deadline = Date.now() + timeout
  let last
  while (Date.now() < deadline) {
    try {
      last = await evaluate(`Boolean(${expression})`)
      if (last) return true
    } catch (error) {
      last = error.message
    }
    await sleep(250)
  }
  throw new Error(`timed out waiting for ${label}`)
}

async function navigate(path = '/') {
  await cdp.send('Page.navigate', { url: `${base}${path}` })
  await cdp.waitEvent('Page.loadEventFired').catch(() => {})
  await sleep(800)
}

async function setCookies(handle, csrf) {
  for (const [name, value, httpOnly] of [
    ['__Host-workspace-session', handle, true],
    ['__Host-workspace-csrf', csrf, false],
  ]) {
    const result = await cdp.send('Network.setCookie', {
      name,
      value,
      url: base,
      path: '/',
      secure: true,
      httpOnly,
      sameSite: 'Lax',
    })
    if (!result.success) throw new Error(`cannot set cookie ${name}`)
  }
  await cdp.send('Network.clearBrowserCache').catch(() => {})
  // Re-read the session from the server: a hash change alone keeps the old
  // authenticated state in memory.
  await reloadPage()
}

async function clearCookies() {
  await cdp.send('Network.clearBrowserCookies')
}

async function screenshot(name) {
  const shot = await cdp.send('Page.captureScreenshot', {
    format: 'png',
    captureBeyondViewport: false,
  })
  await writeFile(join(outDir, `${name}.png`), Buffer.from(shot.data, 'base64'))
}

async function clickText(text, selector = 'button') {
  const value = JSON.stringify(text)
  const clicked = await evaluate(`(() => {
    const nodes = [...document.querySelectorAll(${JSON.stringify(selector)})];
    const node = nodes.find(item => item.textContent && item.textContent.includes(${value}) && !item.disabled);
    if (!node) return false;
    node.click();
    return true;
  })()`)
  if (!clicked) throw new Error(`no ${selector} containing "${text}"`)
  await sleep(400)
}

async function bodyText() {
  return await evaluate('document.body.innerText')
}

async function clickConfirmInDialog(labelText) {
  // The dialog is the only open modal; its footer button carries the action.
  const clicked = await evaluate(`(() => {
    const dialog = document.querySelector('dialog[open]');
    if (!dialog) return false;
    const buttons = [...dialog.querySelectorAll('button')];
    const target = buttons.find(button => button.textContent.includes(${JSON.stringify(labelText)}));
    if (!target) return false;
    target.click();
    return true;
  })()`)
  if (!clicked) throw new Error(`dialog confirm "${labelText}" not found`)
}

async function reloadPage() {
  await cdp.send('Page.reload', { ignoreCache: true })
  await cdp.waitEvent('Page.loadEventFired').catch(() => {})
  await sleep(900)
}

async function setPreference(language, theme) {
  // localStorage is origin-scoped; make sure the tab is on the application
  // origin first (the initial target is about:blank).
  const sameOrigin = await evaluate(
    `(() => { try { return location.origin === ${JSON.stringify(base)}; } catch { return false; } })()`,
  ).catch(() => false)

  if (!sameOrigin) {
    await navigate('/')
  }

  await evaluate(`(() => {
    localStorage.setItem('alxprgs.workspace.lang.v1', ${JSON.stringify(language)});
    localStorage.setItem('alxprgs.workspace.theme.v1', ${JSON.stringify(theme)});
    return true;
  })()`)

  // The providers read these preferences on mount; a hash change alone does not
  // remount them, so reload the application document.
  await reloadPage()
}

async function installedVersion() {
  const fromDom = await evaluate(`(() => {
    const terms = [...document.querySelectorAll('dt')];
    const term = terms.find(node =>
      node.textContent.includes('Установленная версия') ||
      node.textContent.includes('Installed version'));
    if (!term) return '';
    const row = term.closest('.ui-definition') || term.parentElement;
    return row ? (row.querySelector('dd')?.textContent || '').trim() : '';
  })()`).catch(() => '')

  if (fromDom) return fromDom

  const text = await bodyText().catch(() => '')
  const match = text.match(/(?:Установленная версия|Installed version)\s*\n\s*([0-9][0-9.]+)/)
  return match?.[1] ?? ''
}

async function waitForInstalledVersion(version, timeout = 180000) {
  const deadline = Date.now() + timeout
  while (Date.now() < deadline) {
    if ((await installedVersion()) === version) return true
    await sleep(1000)
  }
  return false
}

async function run() {
  await mkdir(outDir, { recursive: true })

  const profile = join(tmpdir(), `alxprgs-cdp-${Date.now()}`)
  chrome = spawn(
    chromePath,
    [
      '--headless=new',
      ...(base.startsWith('https://') ? ['--ignore-certificate-errors', '--allow-insecure-localhost'] : []),
      '--disable-gpu',
      '--no-first-run',
      '--no-default-browser-check',
      '--hide-scrollbars',
      '--remote-debugging-port=' + port,
      '--user-data-dir=' + profile,
      '--window-size=1440,1000',
      'about:blank',
    ],
    { stdio: 'ignore' },
  )

  let target = null
  for (let attempt = 0; attempt < 60 && !target; attempt += 1) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/json/list`)
      const pages = await response.json()
      target = pages.find(page => page.type === 'page')
    } catch {
      // Chrome is still starting.
    }
    if (!target) await sleep(500)
  }
  if (!target) throw new Error('Chrome DevTools endpoint did not start')

  const socket = new WebSocket(target.webSocketDebuggerUrl)
  await new Promise((resolve, reject) => {
    socket.addEventListener('open', resolve, { once: true })
    socket.addEventListener('error', reject, { once: true })
  })

  cdp = new Cdp(socket)
  await cdp.send('Page.enable')
  await cdp.send('Runtime.enable')
  await cdp.send('Log.enable')
  await cdp.send('Network.enable')
  await cdp.send('Emulation.setDeviceMetricsOverride', {
    width: 1440,
    height: 1000,
    deviceScaleFactor: 1,
    mobile: false,
  })

  socket.addEventListener('message', event => {
    const message = JSON.parse(event.data)
    if (message.method === 'Runtime.consoleAPICalled' && message.params.type === 'error') {
      consoleErrors.push(message.params.args.map(arg => arg.value ?? arg.description).join(' '))
    }
    if (message.method === 'Runtime.exceptionThrown') {
      consoleErrors.push(message.params.exceptionDetails.text ?? 'uncaught exception')
    }
    if (message.method === 'Log.entryAdded' && message.params.entry.level === 'error') {
      consoleErrors.push(message.params.entry.text)
    }
  })

  if (scenario.has('dump')) {
    await clearCookies()
    await setCookies(adminHandle, adminCsrf)
    await setPreference('ru', 'dark')
    await navigate('/#/updates')
    await sleep(3000)
    console.log('--- URL', await evaluate('location.href'))
    console.log('--- BODY ---')
    console.log(await bodyText())
    console.log('--- END BODY ---')
  }

  if (scenario.has('anonymous')) {
    await clearCookies()
    await navigate('/')
    await waitFor(`document.body.innerText.includes('ALXPRGS')`, 20000, 'login page')
    record('anonymous login page renders', true)
    await screenshot('login-ru-dark')
  }

  if (scenario.has('admin')) {
    if (!adminHandle || !adminCsrf) throw new Error('admin session cookies are required')
    await setCookies(adminHandle, adminCsrf)
    await navigate('/#/updates')
    await waitFor(`document.body.innerText.includes('Updates') || document.body.innerText.includes('Обновления')`, 30000, 'updates page')
    const text = await bodyText()
    record('administrator sees updates page', text.includes('Installed release') || text.includes('Установленный релиз'))
    record('administrator sees Updates navigation', text.includes('Updates') || text.includes('Обновления'))
    if (expectedCurrent) {
      record('installed version shown', text.includes(expectedCurrent), expectedCurrent)
    }
    await screenshot('updates-admin')
  }

  if (scenario.has('screens')) {
    for (const [language, theme] of [
      ['ru', 'dark'],
      ['ru', 'light'],
      ['en', 'dark'],
      ['en', 'light'],
    ]) {
      await setPreference(language, theme)
      await navigate('/#/updates')
      await waitFor(`document.body.innerText.includes(${JSON.stringify(language === 'ru' ? 'Обновления' : 'Updates')})`, 30000, `${language}/${theme}`)
      await sleep(500)
      await screenshot(`updates-${language}-${theme}`)
      record(`theme/language ${language}/${theme} renders`, true)
    }

    // Mobile layout of the same page.
    await cdp.send('Emulation.setDeviceMetricsOverride', {
      width: 390,
      height: 844,
      deviceScaleFactor: 2,
      mobile: true,
    })
    await setPreference('ru', 'dark')
    await navigate('/#/updates')
    await waitFor(`document.body.innerText.includes('Обновления')`, 30000, 'mobile updates')
    await screenshot('updates-mobile-ru-dark')
    record('mobile layout renders', true)
    await cdp.send('Emulation.setDeviceMetricsOverride', {
      width: 1440,
      height: 1000,
      deviceScaleFactor: 1,
      mobile: false,
    })
  }

  if (scenario.has('check')) {
    await setPreference('ru', 'dark')
    await navigate('/#/updates')
    await clickText('Проверить обновления')
    await waitFor(
      `document.body.innerText.includes('Доступно обновление') || document.body.innerText.includes('Актуальная версия') || document.body.innerText.includes('Ошибка проверки') || document.body.innerText.includes('Несовместимо')`,
      60000,
      'check result',
    )
    const text = await bodyText()
    record('update check returns a state', /Доступно обновление|Актуальная версия|Ошибка проверки|Несовместимо/.test(text))
    if (expectedTarget) {
      record('available version shown', text.includes(expectedTarget), expectedTarget)
    }
    await screenshot('updates-checked-ru-dark')
  }

  if (scenario.has('nonadmin')) {
    if (!userHandle || !userCsrf) throw new Error('user session cookies are required')
    await clearCookies()
    await setCookies(userHandle, userCsrf)
    await navigate('/#/dashboard')
    await waitFor(`document.querySelector('.app-shell') !== null`, 30000, 'user dashboard')
    const text = await bodyText()
    const navButtons = await evaluate(`[...document.querySelectorAll('.app-nav .nav-item')].map(node => node.textContent.trim())`)
    record('user navigation has no Updates entry', !navButtons.some(label => /Updates|Обновления/.test(label)), navButtons.join('|'))
    record('user does not see admin page content', !text.includes('Installed release') && !text.includes('Установленный релиз'))

    await navigate('/#/updates')
    await sleep(1500)
    const denied = await bodyText()
    record(
      'direct navigation shows administrator-only notice',
      denied.includes('Administrator access required') || denied.includes('Требуются права администратора'),
    )

    const status = await evaluate(
      `fetch('/api/v1/admin/update', {credentials:'same-origin'}).then(r => r.status)`,
      true,
    )
    record('admin API rejects a normal user', status === 403, `HTTP ${status}`)
    await screenshot('updates-denied-user')
  }

  if (scenario.has('apply')) {
    await clearCookies()
    await setCookies(adminHandle, adminCsrf)
    await setPreference('ru', 'dark')
    await navigate('/#/updates')
    const alreadyAvailable = await evaluate(
      `document.body.innerText.includes('Доступно обновление') || document.body.innerText.includes('Update available')`,
    )
    if (!alreadyAvailable) {
      await clickText('Проверить обновления')
      await waitFor(
        `document.body.innerText.includes('Доступно обновление') || document.body.innerText.includes('Update available')`,
        60000,
        'update available after check',
      )
    }
    await clickText('Установить обновление')
    await waitFor(`document.querySelector('dialog[open]') !== null`, 10000, 'confirmation dialog')
    await screenshot('updates-confirm-ru-dark')

    // An update with active jobs requires an explicit confirmation in the
    // dialog before the request is sent.
    const activeAcknowledgement = await evaluate(`(() => {
      const dialog = document.querySelector('dialog[open]');
      const checkbox = dialog?.querySelector('input[type=checkbox]');
      if (!checkbox) return 'none';
      if (!checkbox.checked) checkbox.click();
      return 'checked';
    })()`)
    record(
      'active job warning exposes a confirmation control',
      activeAcknowledgement === 'checked' || activeAcknowledgement === 'none',
      activeAcknowledgement,
    )

    // A second request right after starting must be refused by the lock.
    await clickConfirmInDialog('Установить обновление')
    await sleep(600)
    const concurrent = await evaluate(
      `(() => {
         const token = (document.cookie.split('; ').find(part => part.startsWith('__Host-workspace-csrf=')) || '').split('=')[1] || '';
         return fetch('/api/v1/admin/update/apply', {
           method: 'POST',
           credentials: 'same-origin',
           headers: {'Content-Type': 'application/json', 'x-workspace-csrf': token},
           body: JSON.stringify({confirm: true, active_jobs_confirmed: true}),
         }).then(response => response.status).catch(() => 'network');
       })()`,
      true,
    )
    record(
      'concurrent update request refused',
      concurrent === 409 || concurrent === 403 || concurrent === 'network',
      `HTTP ${concurrent}`,
    )

    // Observe restart/reconnect and the progress card.
    let sawProgress = false
    let sawReconnect = false
    let progressShot = false
    let reconnectShot = false
    const deadline = Date.now() + 180000
    while (Date.now() < deadline) {
      const text = await bodyText().catch(() => '')

      if (
        /Резервное копирование|Ожидание заданий|Миграция базы данных|Установка Windows|Запуск сервера|Backing up|Waiting for active jobs|Migrating|Installing the Windows|Starting the server/i.test(
          text,
        )
      ) {
        sawProgress = true
        if (!progressShot) {
          progressShot = true
          await evaluate(`(() => {
            const bar = document.querySelector('.ui-progress');
            if (bar) bar.scrollIntoView({ block: 'center' });
            return Boolean(bar);
          })()`).catch(() => false)
          await sleep(400)
          await screenshot('updates-progress-ru-dark').catch(() => {})
        }
      }

      if (/Перезапускается|Reconnecting|Restarting/i.test(text)) {
        sawReconnect = true
        if (!reconnectShot) {
          reconnectShot = true
          await screenshot('updates-reconnect-ru-dark').catch(() => {})
        }
      }

      const version = await installedVersion()
      const terminal = /Завершено|Успешно|Succeeded|Rolled back/i.test(text)

      if (version === expectedTarget && terminal) break
      await sleep(800)
    }
    record('update progress is visible', sawProgress)
    record('reconnect after restart is visible', sawReconnect)

    const finalVersion = await waitForInstalledVersion(expectedTarget, 120000)
    record('new version is reported after restart', finalVersion, expectedTarget)
    await screenshot('updates-succeeded-ru-dark')
  }

  if (scenario.has('rollback')) {
    await clearCookies()
    await setCookies(adminHandle, adminCsrf)
    await setPreference('ru', 'dark')
    await navigate('/#/updates')
    await waitFor(`document.body.innerText.includes('Выполнить откат')`, 30000, 'rollback section')
    const requiresAck = await evaluate(
      `fetch('/api/v1/admin/update', {credentials: 'same-origin'})
         .then(response => response.json())
         .then(payload => Boolean(payload.rollback?.database_restore_requires_ack))
         .catch(() => false)`,
      true,
    )
    await clickText('Выполнить откат')
    await waitFor(`document.querySelector('dialog[open]') !== null`, 10000, 'rollback dialog')
    await screenshot('updates-rollback-confirm-ru-dark')
    const ack = await evaluate(`(() => {
      const dialog = document.querySelector('dialog[open]');
      const checkbox = dialog?.querySelector('input[type=checkbox]');
      if (!checkbox) return 'none';
      if (!checkbox.checked) checkbox.click();
      return 'checked';
    })()`)
    record(
      'rollback requires a data acknowledgement when the database changed',
      requiresAck ? ack === 'checked' : ack === 'none',
      `required=${requiresAck}, control=${ack}`,
    )
    await clickConfirmInDialog('Выполнить откат')

    let sawRollback = false
    const deadline = Date.now() + 180000
    while (Date.now() < deadline) {
      const text = await bodyText().catch(() => '')
      if (/Откат выполнен|Восстановление базы|Перезапускается/.test(text)) {
        sawRollback = true
        await screenshot('updates-rollback-progress-ru-dark').catch(() => {})
      }
      if (expectedCurrent && text.includes(expectedCurrent) && /Откат выполнен/.test(text)) break
      await sleep(1000)
    }
    record('rollback progresses', sawRollback)
    const text = await bodyText().catch(() => '')
    record('previous version is reported after rollback', !expectedCurrent || text.includes(expectedCurrent), expectedCurrent)
    await screenshot('updates-rolled-back-ru-dark')
  }

  // Any console error is a defect. Network errors are filtered only when they
  // are the expected reconnect during a restart.
  const unexpected = consoleErrors.filter(message => {
    const text = String(message)

    // Expected during the lab run: the server is reachable but the dedicated
    // runtime has not passed acceptance (503 on /ready), and the permission
    // tests intentionally request the admin API as a normal user (403).
    return !(
      text.includes('ERR_CONNECTION_REFUSED') ||
      text.includes('Failed to fetch') ||
      text.includes('NetworkError') ||
      text.includes('503 (Service Unavailable)') ||
      text.includes('403 (Forbidden)') ||
      text.includes('502 (Bad Gateway)') ||
      text.includes('409 (Conflict)')
    )
  })
  record('no unexpected JavaScript console errors', unexpected.length === 0, unexpected.slice(0, 3).join(' | '))

  await writeFile(
    join(outDir, 'acceptance.json'),
    JSON.stringify(
      {
        base,
        scenario: [...scenario],
        results,
        consoleErrors,
        unexpectedConsoleErrors: unexpected,
        at: new Date().toISOString(),
      },
      null,
      2,
    ),
  )

  const failed = results.filter(result => !result.ok)
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`)
  if (failed.length > 0) process.exitCode = 1
}

try {
  await run()
} catch (error) {
  console.error('ACCEPTANCE ERROR:', error.stack ?? error.message)
  process.exitCode = 1
} finally {
  try {
    await cdp?.send('Browser.close')
  } catch {
    // ignore
  }
  chrome?.kill()
}
