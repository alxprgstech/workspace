# ALXPRGS Workspace — интерфейс и дизайн-система

Документ фиксирует, как Workspace применяет **ALXPRGS Design Language v1**,
откуда взяты токены и брендовые ресурсы, как устроены локализация, темы,
анимации и навигация, и какими проверками это подтверждено.

## 1. Источник дизайн-системы

| Что | Источник |
|---|---|
| Семантические токены | ALXPRGS SSO, `frontend/public/theme/design-tokens.css` (ревизия `2c0cf890`) |
| Поведение темы | ALXPRGS SSO, `frontend/public/theme/theme.js` (адаптировано) |
| Логотип и знак | ALXPRGS SSO, `frontend/public/brand/` (`logo-mark.svg`, `logo-adaptive.svg`, `source-manifest.json`) |
| Шрифты | `geist` 1.7.2: `Geist-Variable.woff2`, `GeistMono-Variable.woff2` |
| Лицензия шрифта | SIL Open Font License 1.1, `web/public/licenses/Geist-OFL.txt` |

Токены перенесены без второго независимого набора цветов. Палитра dark-first:
`--canvas #090a0c`, `--surface-1 #0f1115`, `--surface-2 #14171d`,
`--surface-3 #1a1e26`, `--brand #3b82f6`; light: `--canvas #f7f8fa`,
`--surface-1 #ffffff`, `--brand #2563eb`. Полный набор — в начале
`web/src/styles.css`.

Геометрия: боковая навигация 240 px, рабочая ширина 1320 px, обычные контролы
44 px, компактные 36 px, радиусы 6–18 px.

## 2. Брендинг и ресурсы

- `web/public/brand/logo-mark.svg` — знак в шапке и на экране входа.
- `web/public/brand/logo-adaptive.svg` — полный логотип на экране входа.
- `web/public/favicon.svg` — favicon.
- `web/public/fonts/` — Geist Sans и Geist Mono, локально; внешних CDN нет.
- `web/public/licenses/` — лицензия шрифта.
- `web/public/theme-boot.js` — тема и язык до первого кадра.

Все файлы отдаются встроенным Rust-сервером (`web.rs`): закрытые префиксы
`/brand/*`, `/fonts/*`, `/licenses/*`, `/favicon.svg`, `/theme-boot.js`.
Произвольные корневые пути по-прежнему возвращают 404.

## 3. Структура интерфейса

```
web/src/
  components/ui.tsx        Button, IconButton, Card, Badge, Alert, Field,
                           Select, Table, EmptyState, Skeleton, Dialog,
                           ConfirmDialog, Tooltip, Toast, CopyButton
  components/Layout.tsx    верхняя панель, боковая навигация, мобильное меню
  components/CommandPalette.tsx  Ctrl/Cmd+K: разделы, тема, язык, refresh
  components/Brand.tsx     официальный знак и логотип
  pages/                   Dashboard, Workspaces, Jobs, Files, Artifacts,
                           Settings, Login
  i18n/                    типизированные словари ru/en и провайдер
  theme/                   чистая логика темы + React-хук
  router/                  hash-роутинг + хук
  styles.css               токены, компоненты, layout, motion, responsive
```

Диалоги используют нативный `<dialog>`: focus trap, inert-фон и Escape
предоставляет платформа. Разрушающие действия (удаление workspace, отзыв
артефакта) подтверждаются `ConfirmDialog`, а не `window.confirm`.

## 4. Локализация

- `web/src/i18n/en.ts` — эталонный словарь; `ru.ts` типизирован как
  `Messages`, поэтому пропущенный или лишний ключ — ошибка компиляции.
- `web/src/i18n/index.ts` — интерполяция `{name}`, plural через
  `Intl.PluralRules`, форматирование дат/чисел/размеров/длительности через
  `Intl`, локализованные подписи стабильных состояний API.
- `web/src/i18n/provider.tsx` — контекст, мгновенное переключение без
  перезагрузки, сохранение выбора в `localStorage`, обновление
  `document.documentElement.lang`.
- Выбор языка: сохранённый явно → `navigator.languages` (ru → русский,
  иначе английский). Технические идентификаторы, scope-коды, команды,
  stdout/stderr, SHA-256 и JSON-коды не переводятся.
- Серверная страница согласия `/oauth/consent` локализуется по `?lang=ru|en`
  или `Accept-Language` и сохраняет точные scope-коды, CSRF и проверки
  авторизации.

## 5. Темы

- Режимы: `dark`, `light`, `system`; ключ
  `alxprgs.workspace.theme.v1`; старый ключ `workspace-theme` мигрирует.
- `theme-boot.js` применяет тему до первой отрисовки, поэтому вспышки
  неправильной темы нет.
- В режиме `system` слушается `prefers-color-scheme`; изменения из другой
  вкладки подхватываются через событие `storage`.
- `meta[name="theme-color"]` и `color-scheme` обновляются вместе с темой.

## 6. Анимации

| Токен | Длительность | Применение |
|---|---|---|
| `--motion-micro` | 120 ms | hover/pressed кнопок, строки таблиц |
| `--motion-state` | 160 ms | переключение табов, фокус, тема |
| `--motion-enter` | 220 ms | появление страницы, диалога, toast |
| `--motion-layout` | 280 ms | перемещение индикатора активного пункта, прогресс квот |

Анимации объясняют изменение состояния. `prefers-reduced-motion: reduce`
отключает перемещения, пульсацию skeleton и переходы. Постоянных тяжёлых
декоративных эффектов на рабочих страницах нет.

## 7. Навигация

- Hash-роутинг (`#/workspaces`, `#/workspaces/<id>`, `#/jobs?workspace=<id>`)
  сохраняет back/forward и refresh, не перехватывая API/auth/OAuth/MCP/artifact
  маршруты встроенного сервера.
- Палитра команд: `Ctrl+K` / `Cmd+K`, кнопка «Палитра команд» в шапке,
  стрелки/Enter/Escape, поиск по локализованным названиям, безопасные действия
  (навигация, тема, язык, обновление). Удаление и exec в палитру не входят.
- Страницы после первого открытия остаются смонтированными (скрытыми), поэтому
  выбранный workspace, фильтр заданий, открытый файл и черновик не теряются при
  переходах и фоновом обновлении.
- Раздел «Обновления» (`#/updates`) виден и доступен в палитре только
  администратору. Он показывает установленный/доступный релиз, build SHA,
  совместимость, changelog, live-журнал этапов и прогресс, reconnect после
  перезапуска и безопасный откат с подтверждением активных задач/восстановления
  БД. Сервер повторно проверяет роль на каждом запросе; не-администратору
  прямой API возвращает 403. Скриншоты — `docs/screenshots/updates/`.

## 8. Доступность

- Видимый `:focus-visible`, семантические заголовки и `aria-label`, skip-link.
- Нативные диалоги с возвратом фокуса, мобильное меню в диалоге.
- Контраст проверен axe (WCAG 2.2 AA); статусы различаются не только цветом
  (текстовая подпись + точка).
- Таблицы прокручиваются внутри контейнера; страница не имеет горизонтального
  скролла от 320 px.

## 9. Проверка

```bash
cd web
npm ci
npm run typecheck
npm test          # 15 unit-тестов: словари, plural, форматирование, тема, роутинг
npm run build
npm audit
```

Браузерная приёмка выполнена локально через Playwright с полностью
синтетическими ответами API (без реального аккаунта, токенов и логов):
десктоп 1440×900 и мобильный 390×844, RU/EN, dark/light/system,
палитра команд, клавиатура, reduced motion, axe.

Результат: **28 проверок, 0 ошибок, 0 ошибок консоли**; скриншоты —
`docs/screenshots/`:

| Файл | Что показывает |
|---|---|
| `dashboard-ru-dark.png` | Обзор, RU, тёмная тема |
| `workspaces-ru-dark.png` | Рабочие пространства и форма создания |
| `workspace-detail-ru-dark.png` | Карточка пространства с квотами |
| `jobs-ru-dark.png` | Задания, статусы и логи |
| `files-ru-dark.png` | Файлы и breadcrumb |
| `artifacts-ru-dark.png` | Артефакты, SHA-256 и статусы |
| `settings-ru-dark.png` | Настройки, Runtime, сервисы и модули |
| `palette-ru-dark.png` | Палитра команд |
| `dashboard-en-light.png`, `artifacts-en-light.png` | Английский, светлая тема |
| `login-ru-dark.png`, `login-en-light.png` | Экран входа |
| `mobile-dashboard-ru-dark.png`, `mobile-nav-ru-dark.png` | Мобильный вид и меню |

Данные на скриншотах — синтетические (`demo-alpha`, `demo-beta`, демонстрационные
идентификаторы). Реальные учётные записи, токены и приватные логи не
используются.

## 10. Что сохранено

- `web/src/api.ts` не менялся по контрактам: cookie-сессия, CSRF-заголовок,
  обработка 401/503, все серверные ограничения остаются на backend.
- OAuth/SSO-токены в `localStorage` не хранятся; там только язык и тема.
- Внешние CDN, шрифты и скрипты не подключены; CSP/CSRF не ослаблены.
- Сборка Vite встраивается через RustEmbed; Node.js на сервере не требуется.
