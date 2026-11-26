# ALXPRGS Workspace — приёмка обновления и релизов 0.2.0

> **Статус на 26.11.2026, 20:30 MSK: задачи #17, #18 и #19 реализованы и
> проверены на изолированном стенде. Production намеренно НЕ обновлялся и
> продолжает работать на 0.1.0 / schema 9; обновление выполняется только после
> отдельного разрешения владельца.** Публикация production-релиза не
> выполнялась.

## 1. Что сдано

| Issue | Результат | Основные доказательства |
| --- | --- | --- |
| #17 — безопасное обновление и откат | Готово | единый updater для CLI и Web UI; journal + lock; preflight; backup; миграция 9→11; замена exe и WSL-компонентов; автоматический и ручной откат; recovery после прерывания |
| #18 — воспроизводимые пакеты и релизы | Готово | подписанный пакет `0.2.0` для Windows x64 + static-pie musl helper/egress; release workflow реально запущен (TEST-подпись, без публикации) |
| #19 — раздел обновлений в Web UI | Готово | страница `Updates`, RU/EN, light/dark, mobile, live-прогресс, reconnect, подтверждения active jobs и restore БД, скриншоты |

Работа выполнена прямо в `main`; созданы подписанные коммиты:
`54ac3e1` (движок и CLI), `5cd5de2` (Web UI), `d3ddf23` (сборка/релиз/доки),
`bc56f17` и `ea2416b` (исправления, найденные при реальном запуске workflow).

## 2. Финальный артефакт

- Файл: `dist/workspace-0.2.0-windows-x64.zip`
- Версия: **0.2.0**, канал `stable`
- Коммит: `ea2416bc08d6cd8c3241e5ef4b63a9dde40ecac6` (чистое дерево)
- SHA-256 пакета: `ba151fd27008d4c1b81527d5b131d03b741fc5db8df64846eeb1b418fa575efc`
- SHA-256 манифеста: `602b0eb43b3262cd2ed18160a3a5770c98146e59c7742c31b60144aab5050f37`
- Подпись Ed25519 проверена встроенным публичным ключом
  `deploy/release-public-key.txt`.
- Контрольная сумма: `dist/workspace-0.2.0-windows-x64.zip.sha256`;
  сводка: `dist/release.json`.
- Состав: `workspace.exe`, `linux/workspace-helper`, `linux/workspace-egress`,
  5 скриптов `deploy/windows/`, `UPDATE.md`, `RELEASES.md`, манифест и подпись
  (10 файлов). Все linux-бинарники — `static-pie`, без `INTERP`, проверено
  `readelf` в WSL.

Артефакт собран из чистого коммита и затем **реально установлен** на
изолированный стенд: `/health` вернул `build_sha=ea2416b…`, `installed.json`
записал тот же SHA. Это доказывает совпадение манифеста, бинарника и коммита.

## 3. Что реализовано

- **Единый движок** `workspace-server::update` для CLI и HTTP API: проверка
  пакета, совместимости схемы/платформы/updater-format, подписи и хэшей каждого
  файла; доверенный ключ встроен в бинарник.
- **Транзакция**: drain → резервная копия (SQLite `VACUUM INTO` на работающей
  БД, exe, config, WSL-компоненты) → остановка сервера → замена WSL-компонентов
  → миграция → замена exe → запуск → проверка health (версия, build SHA, schema).
- **Отказоустойчивость**: journal `state/update/journal.json` (+`.prev`),
  lock с PID, recovery прерванной операции, автоматический откат при ошибке
  health, сохранение post-update БД, safety copy при ручном откате.
- **Windows-замена**: detached updater ждёт EOF launcher-а, чтобы отпустился
  образ; сервер остаётся доступен для live-прогресса во время drain/backup,
  затем updater сам запрашивает контролируемую остановку.
- **Секреты**: `WORKSPACE_SSO_CLIENT_SECRET` и `WORKSPACE_AUTH_KEY` сохраняются
  через DPAPI в `state/secrets.dat`; после обновления сервер стартует без
  повторного ввода.
- **CLI**: `workspace --version`, `workspace update --check`,
  `workspace update --status`, `workspace update [--package PATH]`,
  `workspace update --rollback`, `workspace update --bootstrap`; release-команды
  `release keygen|sign|package|verify`.
- **Web UI**: раздел «Обновления» только для администратора; установленная и
  доступная версии, build SHA, changelog, совместимость, последняя проверка,
  журнал этапов и прогресс, reconnect после перезапуска, подтверждение активных
  задач, подтверждение восстановления БД, безопасный откат.
- **Release**: `scripts/build-release.ps1` (Windows + Docker musl, allowlist,
  deterministic ZIP, подпись, самопроверка), `scripts/build-linux-musl.sh`,
  `.github/workflows/release.yml` (tags/manual, Windows/Linux/package/publish).

## 4. Проверка CLI (реальные запуски, exit code 0)

```text
workspace --version
ALXPRGS Workspace 0.2.0
build: ea2416bc08d6cd8c3241e5ef4b63a9dde40ecac6

workspace update --check --package dist/workspace-0.2.0-windows-x64.zip
Installed: 0.2.0 (ea2416b…)
Schema: 11
Available: 0.2.0 (ea2416b…) channel stable runtime wsl2/none
Compatibility: schema 9..=11
Status: UP_TO_DATE

workspace update --status
Installed: 0.2.0 (ea2416b…), schema 11
Active jobs: 0
Update journal: rolled_back (100%)
Message: installation recovered at version 0.2.0
Rollback available: 0.2.1 -> 0.2.0
```

> `workspace --version` без `--config` показывает **detected default
> installation** (на этой машине — production 0.1.0 / schema 9), а не
> лабораторный экземпляр. Для стенда используйте
> `workspace update --status --install-dir <lab>`.

## 5. Изолированный E2E (не mock)

Стенд: `%LOCALAPPDATA%\ALXPRGS\Workspace-Lab`, порт 8799, WSL-пути
`/srv/alxprgs-workspace-lab/*`; production не затрагивался.

| Сценарий | Результат |
| --- | --- |
| Центральное обновление 0.2.0 → 0.2.1 | `succeeded`, все 8 этапов completed |
| Миграция schema 9 → 11 | instance ID, пользователи, настройки и workspace сохранены |
| Замена WSL helper/egress/quota/sudoers | хэши изменились на 0.2.1; после отката вернулись 0.2.0 |
| Остановка сервера | graceful shutdown watcher: 0.44 с |
| Повторный старт без env | DPAPI-секреты загружены, SSO `enabled` |
| Ручной откат 0.2.1 → 0.2.0 | `rolled_back`, exe/БД/WSL восстановлены, сервер запущен |
| Прерывание updater на `install_runtime` | journal `interrupted (40%)`, lock освобождён, recovery-rollback `rolled_back`, сообщение `installation recovered at version 0.2.0` |
| Health-failure (порт занят) | `start failed` + автоматический `rollback completed`; сервер 0.2.0 здоров |
| Изменение БД после обновления | `database_restore_requires_ack=true`; UI требует checkbox; safety copy содержит sentinel, восстановленная БД — нет |
| Legacy 0.1.0 → 0.2.0 (Lab B) | bootstrap с schema 9→11 и сохранением строк; откат на 0.1.0 |
| Повторная установка финального чистого пакета | `0.2.0 -> 0.2.0`, health `ea2416b…` |

Production не обновлялся; его health и публичный HTTPS проверены отдельно
(см. §8).

## 6. Web UI — сквозные проверки

CDP-автоматизация в реальном Chrome (HTTPS-стенд с локальным сертификатом,
`http://localhost:8443` → 8799):

- `admin,screens,check`: 11/11 — страница и навигация только для админа,
  установленная версия, 4 комбинации RU/EN × light/dark, mobile, проверка
  обновления, отсутствие неожиданных console errors.
- `apply`: **6/6** — предупреждение об активной задаче с checkbox, конкурентный
  запрос `409`, **live-прогресс** («Резервное копирование · 20%», этапы),
  reconnect-баннер, новая версия после перезапуска, без ошибок.
- `rollback`: **4/4** — при изменённой БД требуется подтверждение, прогресс,
  возврат на 0.2.0, без ошибок.
- Non-admin: пункт навигации скрыт, прямой API — `403`.

Скриншоты: `docs/screenshots/updates/` —
`updates-ru-dark.png`, `updates-ru-light.png`, `updates-en-dark.png`,
`updates-en-light.png`, `updates-mobile-ru-dark.png`, `updates-progress-ru-dark.png`,
`updates-reconnect-ru-dark.png`, `updates-succeeded-ru-dark.png`,
`updates-rollback-confirm-ru-dark.png`, `updates-rolled-back-ru-dark.png`,
`updates-denied-user.png`.

## 7. Release workflow — реальный запуск

Workflow `.github/workflows/release.yml` запускался через `workflow_dispatch`
на `ea2416b`, с TEST-подписью и **без публикации**:
<https://github.com/alxprgstech/workspace/actions/runs/45862728172>

- Windows x64: success; Linux musl: success; Signed release package: success;
  Publish: skipped (by input).
- Артефакт `workspace-0.2.0-windows-x64-TEST.zip`, SHA-256
  `ac4a7543c3b038ebd67d83a7e42743550ff6dc317cc72f5b557c62e423e60923`;
  манифест содержит `git_sha=ea2416b…-test` — эфемерная подпись и TEST-имя
  исключают путаницу с production-релизом.
- Итерации честно зафиксированы: первый запуск выявил, что `release package`
  проверял TEST-пакет встроенным production-ключом (исправлено в `bc56f17`);
  второй — ошибку приоритета PowerShell в имени TEST-пакета (исправлено в
  `ea2416b`).
- Реальная публикация GitHub Release **не выполнялась**: для неё нужен
  `RELEASE_SIGNING_KEY_PKCS8_BASE64` и отдельное разрешение владельца.

## 8. Quality gates (финальный коммит)

| Проверка | Результат |
| --- | --- |
| `cargo fmt --all -- --check` | exit 0 |
| `cargo test --workspace --all-features --locked` | **329 passed, 0 failed**, 6 ignored |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | exit 0 |
| `git diff --check` | exit 0 |
| `npm ci` | 0 vulnerabilities |
| `npm run typecheck` | exit 0 |
| `npm run build` | exit 0 |
| `npm test` | 15 passed, 0 failed |
| `release verify --package dist/workspace-0.2.0-windows-x64.zip` | OK |
| Production `/health` (127.0.0.1:8765) | 200, 0.1.0, schema 9 |
| Публичный `https://workspace.alxprgs.tech/health` | 200 |

## 9. Ограничения и честные замечания

1. **Production не обновлён** — это осознанное требование. Готовый пакет
   `0.2.0` и bootstrap-команда переданы владельцу; обновление выполняется
   только с его разрешением и после резервной копии.
2. **GitHub-подпись коммитов**: в неинтерактивной сессии агента 1Password
   SSH-agent не смог подписать (`failed to fill whole buffer`), поэтому коммиты
   подписаны локальным ключом `debian-server`; криптографическая подпись
   валидна, но GitHub может показать «Unverified», пока этот публичный ключ не
   добавлен в аккаунт.
3. **Live-прогресс**: этапы drain/backup видны в UI, пока сервер работает;
   после контролируемой остановки страница показывает reconnect, а завершённые
   этапы — после старта. Отдельного progress-сервера на время простоя нет.
4. **CI TEST-сборка** использует эфемерный ключ и не устанавливается на
   production; проверка встроенным ключом для неё ожидаемо отклоняет пакет.
5. **WSL-стенд** имеет `storage.mounted` и `runtime.acceptance` в состоянии
   NOT READY (лабораторный data-root не смонтирован); это ограничение стенда,
   а не результата обновления. Полный runtime-E2E с контейнерами не входил в
   этот прогон.
6. Linux-компоненты собраны и проверены (`static-pie`, успешный CI job), но
   полный сервер на Linux не запускался — целевая платформа 0.2.0 — Windows x64.
7. Нагрузочное/длительное тестирование и проверка обновления с реальными
   активными job-ами (кроме искусственной draining-записи) не проводились.

## 10. Команды для владельца (не выполнены)

Первый bootstrap на production (после резервной копии и с явными секретами):

```powershell
$env:WORKSPACE_SSO_CLIENT_SECRET = '<из хранилища>'
$env:WORKSPACE_AUTH_KEY = '<из хранилища>'
powershell -ExecutionPolicy Bypass -File deploy\windows\bootstrap-update.ps1 `
  -Package 'dist\workspace-0.2.0-windows-x64.zip'
```

Последующие обновления:

```powershell
workspace update --check
workspace update --package 'dist\workspace-0.2.0-windows-x64.zip'
workspace update --rollback --yes --allow-data-restore
```

Bootstrap проверяет подпись, делает backup, мигрирует schema 9→11 и
перезапускает сервер; при ошибке health автоматически откатывается.

## 11. Закрытие issue

- **#17** — закрывается: все команды и сценарии обновления/отката проверены,
  включая прерывание и health-failure.
- **#18** — закрывается: пакет собран, подписан, проверен и установлен; release
  workflow реально отработал (TEST, без публикации).
- **#19** — закрывается: UI-сценарии и скриншоты приложены; non-admin доступ
  закрыт на сервере.

---

## Приложение A. Предыдущая приёмка MVP (историческая)

> Ниже сохранён предыдущий отчёт. Он описывает состояние **до** обновления
> 0.2.0; его утверждения о production относятся к прошлому этапу и не
> отменяют текущий статус: production сейчас 0.1.0 / schema 9 и намеренно не
> обновлялся.

> **Статус: MVP развёрнут и публично доступен.** Действующий экземпляр обновлён
> 26.11.2026 (схема 4 → 8, instance ID сохранён), публичный
> `https://workspace.alxprgs.tech` отвечает 200, MCP-endpoint возвращает
> корректный 401 с metadata. Единственный внешний шаг, который остаётся —
> повторное подключение коннектора ChatGPT (для согласия на новые scopes) и
> один реальный вызов из ChatGPT; в среде агента нет вашей сессии ChatGPT.

## 1. Итоговый статус

- Этапы **001–014** реализованы; единственное исключение — модуль
  `whisper-vad`, который **выключен** из-за отсутствия доверенного источника
  (см. §13).
- Полная практическая приёмка проведена: Runtime, безопасность, MCP, файлы,
  артефакты, egress, перезапуск и восстановление, ClipForge.
- Действующий экземпляр **обновлён и запущен**: `workspace.exe` (SHA-256
  установленного файла совпадает с релизом), БД мигрирована 4 → 8 с
  автоматическим бэкапом `backups\pre-schema4-*`, instance ID
  `f32ac08f-5f8c-41df-8a72-820d9ff123d8` сохранён. Публичный health — 200,
  Protected Resource Metadata — 200, MCP без токена — 401 с корректным
  `WWW-Authenticate`. 502 был вызван отсутствовавшим обратным SSH-туннелём;
  он поднят (`deploy/windows/tunnel.ps1`).
- Реальный вызов из ChatGPT **не выполнялся**: в среде агента нет
  авторизованной сессии. Протокольная E2E-приёмка через настоящий HTTP endpoint
  пройдена (§8).
- Инструмент ClipForge «№004 Speech» **не реализовывался**: в базовом коммите
  `91db3f5…` спецификация Speech отсутствует (проверены `SPECIFICATION.md` и все
  документы `docs/`), а upstream-репозиторий недоступен (HTTP 404). Вместо
  выдумывания требований проведена репрезентативная техническая приёмка (§8.2).

## 2. Выполненные этапы

| Этап | Статус | Доказательство |
| --- | --- | --- |
| 001–007 | завершён | тесты; ранее — реальное подключение ChatGPT |
| 008 Runtime Backend | завершён | `runtime accept` PASSED (3.7 с) |
| 009 Workspace Manager | завершён | runtime E2E + ClipForge + recovery E2E |
| 010 Capability Images | завершён (без whisper-vad) | реальные образы: cargo/rustc/clippy/g++/python/ffmpeg |
| 011 Execution & Jobs | завершён | реальные jobs, exit code, stdout/stderr, отмена |
| 012 Files & Artifacts | завершён | write/read/patch/import/publish/download + SHA-256 |
| 013 Security & Isolation | завершён | изоляция + egress allowlist проверены |
| 014 Финальная приёмка | завершён по внутренним проверкам; внешние шаги — §13 | базовый E2E, ClipForge, MCP protocol E2E, recovery, egress |
| Web UI | завершён | 6 разделов, production-сборка в бинарнике |

## 3. Реализованные функции

- **Runtime**: WSL2/rootless Docker backend, явная конфигурация, проверки
  (rootless, cgroup v2, контроллеры, seccomp, WSL config, ext4+prjquota,
  свободное место, helper), холодный старт, таймауты, единый `RuntimeStatus`,
  автоматическая практическая приёмка изоляции, восстановление после рестарта.
- **Workspaces**: создание/список/инфо/изменение/stop/start/delete, ephemeral и
  persistent, состояния, квоты (блоки и inode), TTL и idle timeout, лимит
  активных, reaper, reconciliation, идемпотентная подготовка образов.
- **Capabilities**: доверенные рецепты в бинарнике, base/git/cpp/rust/python/
  ffmpeg, кеш по canonical key, immutable image ID, последовательные
  ограниченные сборки.
- **Jobs**: argv без shell, cwd/env, очередь, статусы, exit code, логи с
  ограничением 1 МиБ на поток, таймауты, отмена, idempotency, восстановление
  `interrupted`, семафор параллелизма.
- **Files/Artifacts**: список/чтение/запись/строгий patch, импорт вложений
  (`openai/fileParams`) и URL с SSRF-защитой, безопасная распаковка ZIP/TAR
  (traversal, ссылки, бомбы, лимиты, exec-bit), неизменяемые артефакты с
  SHA-256, TTL-ссылки, отзыв, автоматическая очистка.
- **Security**: rootless, cap-drop ALL, no-new-privileges, seccomp, read-only
  rootfs, tmpfs, CPU/RAM/swap/PID limits, отсутствие docker.sock и сети,
  project quotas, изоляция владельцев, scoped OAuth, fail-closed, egress
  allowlist через доверенный прокси.
- **Web UI**: Dashboard, Workspaces, Jobs, Files, Artifacts, Settings.

## 4. MCP-инструменты (21)

`server_info`, `capabilities_list`, `workspace_create`, `workspace_list`,
`workspace_info`, `workspace_update`, `workspace_stop`, `workspace_delete`,
`exec`, `job_status`, `job_logs`, `job_cancel`, `files_list`, `file_read`,
`file_write`, `file_patch`, `file_import`, `url_import`, `artifact_publish`,
`artifact_list`, `artifact_download`.

Scopes: `workspace:read`, `workspace:write`, `workspace:delete`,
`execution:run`, `files:read`, `files:write`, `files:import`,
`artifacts:read`, `artifacts:publish`.

**Исправлен реальный дефект**: scope `workspace:delete` отсутствовал в
каноническом наборе, из-за чего инструмент удаления был недоступен ни одному
OAuth-клиенту. Существующие клиенты должны пройти повторное согласие, чтобы
получить удаление.

## 5. Тесты

- `cargo test --workspace --all-features --locked`: **243 теста, все
  проходят**; 7 ignored — реальные приёмочные тесты, запускаются отдельно.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: 0
  предупреждений.
- `cargo fmt --all --check`, `git diff --check`: чисто.
- Реальные ignored-приёмки (запускались, проходят):
  - `runtime_accept` (CLI) — PASSED;
  - `runtime_e2e` — создание/квоты/job/файлы/артефакты;
  - `image_build` — сборка образов;
  - `clipforge_acceptance` — PASSED;
  - `mcp_e2e` — protocol-level;
  - `egress_e2e` — allowlist;
  - `recovery_e2e` — перезапуск сервера.

## 6. Безопасность: результаты

Подтверждено практическими проверками на выделенном Runtime:

- контейнер: `CapEff=0`, `NoNewPrivs=1`, `Seccomp=2`, read-only rootfs,
  tmpfs, memory 256 МиБ, swap 0, pids 64, CPU quota 50%, uid_map `0 1000 1`;
- нет docker.sock, нет маршрутов и внешней сети при `network = none`;
- ext4 project quotas: реальные `Disk quota exceeded` по блокам и inode;
- изоляция владельцев и scoped-авторизация каждого MCP-инструмента;
- SSRF-защита импорта (HTTPS, публичные адреса, DNS pinning, редиректы);
- egress allowlist: direct=000, allowed=200, denied=403, metadata=405;
  job внутри workspace: `allowed=200`, отказ `403`, сеть удаляется вместе с
  workspace.

Остаточный риск (честно): WSL2 + rootless Docker не являются границей,
эквивалентной виртуальной машине для произвольного враждебного кода.

## 7. Runtime acceptance

`target\release\workspace.exe --config config.toml runtime accept`:

```
Acceptance: PASSED in 3697 ms
```

Проверено: engine, WSL config, ext4+prjquota, helper, контейнерный пробник
(все перечисленные выше ограничения), реальное исчерпание квот.

## 8. E2E-приёмка

### 8.1 Базовая

`runtime_e2e`: создание пространства с квотой → job (exit 3, раздельные
stdout/stderr) → запись/чтение/патч → публикация → скачивание → SHA-256 →
удаление → артефакт пережил удаление.

### 8.2 ClipForge

`clipforge_acceptance` PASSED:

- импорт 3086 записей (62 МБ);
- версия CLI 0.3.0 → 0.4.0 через `file_patch`, обновлён тест проекта;
- `cargo check` ✅; `cargo test --lib` — 45 тестов ✅; `cargo build --release` ✅;
- бинарник в контейнере: `clipforge-cli 0.4.0`;
- артефакт 964 496 байт, скачан, SHA-256 совпал;
- workspace удалён, артефакт сохранён.

Ограничение: интеграционные тесты ClipForge требуют внешних медиа-инструментов
(`whisper-cli`), поэтому тестовый шаг ограничен библиотечными тестами; Speech
не реализовывался (§1, §13).

### 8.3 MCP protocol E2E

`mcp_e2e` PASSED через реальный HTTP listener:

- discovery обоих well-known документов;
- анонимный вызов → 401 + `WWW-Authenticate`;
- `initialize`, `tools/list` (21 инструмент, закрытые схемы, `_meta`,
  `openai/fileParams`);
- `tools/call server_info` — реальный runtime status;
- структурированная ошибка `workspace_not_found`.

### 8.4 Перезапуск и восстановление

`recovery_e2e` PASSED:

- сервер запущен, execution readiness дождались;
- workspace создан, job запущен (`sleep 120`);
- сервер аварийно остановлен;
- сервер поднят на той же БД;
- job → `interrupted` с сообщением «the server restarted while the job was
  active»; workspace сохранился; удаление и очистка прошли; `/health` в норме.

### 8.5 Egress

`egress_e2e` PASSED (см. §6) и `scripts/egress-acceptance.sh` PASSED.

## 9. Версии

| Компонент | Версия |
| --- | --- |
| Приложение | 0.1.0 |
| Схема SQLite | 9 |
| Rust | 1.100.0 |
| Docker Engine | 29.10.1 (rootless) |
| WSL kernel | 6.18.33.2-2 |
| Debian | 13.5 trixie |
| Base image | `debian@sha256:8aa5a7bf5a1955bddbe7d74fcdd0b91ab30e74362a8b73af672b0481b35b44a0` |
| Rust base | `rust:1.100-slim` |

## 10. Release-бинарник и ресурсы

- `target\release\workspace.exe` — **16 041 472 байт**, web UI встроен,
  Node.js в runtime не требуется.
- Простой сервер: **8.5 МиБ** working set, 7 потоков.
- Хост: C: ≈ **14.5 ГиБ** свободно; ext4-хранилище — 21 МБ занято;
  Docker images 2.46 ГБ, build cache 2.37 ГБ.
- Контейнер-пробник: 256 МиБ RAM, 0.5 CPU, 64 PID, swap 0.

## 11. Коммиты

Все коммиты подписаны (`git commit -S`) и отправлены в `origin/main`:
`9b6a9e2`, `7c02afc`, `c3da30a`, `5a182b6`, `0601711`, `2872efb`, `c0bb00c`,
`f2c6d69`, `6cf94e5`, `bbdebdc`, `ea6f9c6`, `9bd83fe`, `11286d5`, `9e062ce`,
`71f5b1a`, `0b9a361`, `2a00008`.

Issues и PR не создавались: репозиторий ведётся прямыми подписанными коммитами
в `main`, искусственные Issues/PR не создавались.

## 12. Запуск и обновление

### Запуск (без обновления действующего экземпляра)

```powershell
target\release\workspace.exe --config config.toml serve
```

### Обновление действующего экземпляра (минимальный шаг)

Нужны два секрета (их нет в окружении агента):

```powershell
$env:WORKSPACE_SSO_CLIENT_SECRET = "<секрет SSO>"
$env:WORKSPACE_AUTH_KEY         = "<base64 32 байта>"
pwsh deploy\windows\install.ps1 -Start
```

Скрипт: останавливает сервер, делает резервную копию бинарника, конфигурации и
согласованный SQLite-снимок, устанавливает новый бинарник, запускает и ждёт
`/health`, печатает команду rollback. БД мигрируется вперёд; instance ID,
пользователи, SSO-сессии, OAuth-клиенты и гранты сохраняются.

### Rollback

```powershell
# путь печатает install.ps1: ...\backups\release-<timestamp>
Copy-Item "<backup>\workspace.exe" "$env:LOCALAPPDATA\ALXPRGS\Workspace\workspace.exe" -Force
Copy-Item "<backup>\workspace.db" "$env:LOCALAPPDATA\ALXPRGS\Workspace\workspace.db" -Force
Start-Process "$env:LOCALAPPDATA\ALXPRGS\Workspace\workspace.exe" -ArgumentList @("--config","$env:LOCALAPPDATA\ALXPRGS\Workspace\config.toml","serve")
```

## 13. Известные ограничения и блокеры

1. **whisper-vad выключен.** Нет доверенного источника с закреплённой
   контрольной суммой; модуль отдаётся как `not_installed`, создание
   пространства с ним отклоняется. Это осознанный fail-closed, а не заглушка.
2. **ClipForge Speech не реализован.** В базовом коммите `91db3f5…`
   спецификация Speech отсутствует (проверены `SPECIFICATION.md`, `docs/*`),
   upstream-репозиторий недоступен (HTTP 404). Требования не выдумывались;
   проведена репрезентативная техническая приёмка (§8.2).
3. **Публичный доступ зависит от SSH-туннеля.** Обратный туннель сейчас
   запущен вручную (`deploy/windows/tunnel.ps1`); после перезагрузки хоста его
   нужно запустить снова либо оформить как задачу планировщика (по вашему
   решению — задача не создавалась без явного запроса).
4. **Реальный вызов из ChatGPT не выполнялся** — нет авторизованной сессии в
   среде агента. Дополнительно: существующий коннектор ChatGPT получал токен со
   старым scope `workspace.read`; чтобы увидеть новые инструменты, его нужно
   переподключить и согласиться на новые scopes (`workspace:write`,
   `workspace:delete`, `execution:run`, `files:*`, `artifacts:*`). После этого
   используйте запрос из §14.
5. **Egress**: разрешён только HTTPS (`CONNECT:443`); plain HTTP и произвольные
   порты запрещены. Это ограничение политики, а не дефект.
6. Остаточный риск платформы: WSL2 + rootless Docker не эквивалентны VM.

## 14. Готовый запрос для финальной проверки из ChatGPT

> Подключись к ALXPRGS Workspace и покажи `server_info` и `capabilities_list`.
> Создай workspace `chatgpt-check` с возможностями `base` и `git`, дождись
> готовности. Через `file_write` создай в нём `hello.txt` с текстом
> `hello from ChatGPT`, запусти `git --version` и `cat hello.txt`, покажи логи.
> Опубликуй `hello.txt` как артефакт (`artifact_publish`), получи ссылку
> (`artifact_download`) и назови SHA-256. В конце удали workspace и подтверди,
> что артефакт остался доступен.

Ожидаемый результат: все шаги выполняются через MCP-инструменты, job
завершается успешно, артефакт скачивается, SHA-256 совпадает, workspace
удаляется.
