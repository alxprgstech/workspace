# ALXPRGS Workspace — статус разработки

> **Статус: обновление 0.2.0 принято на изолированном стенде; production
> намеренно остаётся на 0.1.0 / schema 9.** Итоговый документ —
> `COMPLETION_REPORT.md`. Здесь рабочая хронология и детали предыдущих приёмок;
> новый раздел о 0.2.0 — в `COMPLETION_REPORT.md` и `docs/UPDATE.md`.

## 1. Этапы

| Этап | Состояние | Доказательство |
| --- | --- | --- |
| 001–007 | завершён | тесты; ранее — реальное подключение ChatGPT |
| 008 Runtime Backend | **завершён** | `runtime accept` PASSED на выделенном WSL2 |
| 009 Workspace Manager | **завершён** | реальный E2E + ClipForge acceptance |
| 010 Capability Images | **завершён** (кроме whisper-vad) | образ собран; cargo/rustc/clippy/g++/python/ffmpeg проверены |
| 011 Execution & Jobs | **завершён** | job exit code, stdout/stderr, отмена, idempotency |
| 012 Files & Artifacts | **завершён** | write/read/patch/import/publish/download + SHA-256 |
| 013 Security & Isolation | **завершён** | изоляция/лимиты подтверждены; egress allowlist реализован и проверен |
| 014 Финальная приёмка | частично | runtime E2E, ClipForge, MCP protocol E2E; ChatGPT-сессия и деплой — нет |
| Web UI | **реализован** | 6 разделов, production-сборка встроена в бинарник |
| Update & Release 0.2.0 | **принят на стенде** | подписанный пакет, миграция 9→11, rollback/recovery, release workflow; production не обновлялся |

## 2. Практическая приёмка (реальные прогоны)

### 2.1 Runtime acceptance (`workspace runtime accept`)

PASSED. Проверено на выделенном WSL2:

- engine dedicated/rootless, cgroup v2 + systemd, seccomp;
- WSL config: interop/automount/appendWindowsPath выключены;
- ext4 + prjquota, свободное место, helper (protocol 2, uid 1000);
- реальное исчерпание блочной и inode-квоты;
- контейнерный пробник: CapEff=0, NoNewPrivs=1, Seccomp=2, read-only rootfs,
  нет docker.sock, только loopback, нет маршрутов, memory/swap/pids/cpu limits
  применены, uid_map `0 1000 1`.

### 2.2 ClipForge acceptance — **PASSED**

`cargo test -p workspace-server --test clipforge_acceptance -- --ignored`:

- архив базового коммита `91db3f5…` (3086 записей, 62 МБ) импортирован и
  распакован внутри Workspace;
- структура проекта прочитана, `cli/Cargo.toml` изменён (0.3.0 → 0.4.0) через
  `file_patch`, обновлено ожидание в `cli/tests/cli.rs`;
- `cargo check --offline` — успешно (13.9 с);
- `cargo test --offline --lib` — **45 тестов, все прошли**;
- `cargo build --release --offline` — успешно;
- собранный бинарник запущен в контейнере: `clipforge-cli 0.4.0`;
- артефакт опубликован (964 496 байт), скачан и проверен по SHA-256;
- Workspace удалён, артефакт продолжил существовать.

Ограничение: интеграционные тесты самого ClipForge требуют внешних
медиа-инструментов (`whisper-cli`) и в образе отсутствуют; тестовый шаг
ограничен библиотечными тестами. Инструмент Speech (upstream diff ≈1344 строки)
в этом раунде не переносился — это отдельная задача разработки.

### 2.3 MCP protocol E2E — **PASSED**

`cargo test -p workspace-server --test mcp_e2e -- --ignored` поднимает реальный
HTTP listener (`serve`) на loopback с временной БД и настоящим OAuth-токеном:

- `/.well-known/oauth-protected-resource` — корректные resource/scopes;
- `/.well-known/oauth-authorization-server` — endpoints;
- анонимный `POST /chatgpt/mcp` → 401 + `WWW-Authenticate` с resource_metadata;
- `initialize` по HTTP — OK;
- `tools/list` — **21 инструмент**, закрытые JSON Schema, `_meta` со scopes,
  `openai/fileParams` у `file_import`;
- `tools/call server_info` — реальный статус runtime (в момент прогона engine
  был остановлен, что честно отражено как not ready);
- структурированная ошибка `workspace_not_found` вместо transport-сбоя.

### 2.4 Файлы, артефакты, jobs (E2E)

`runtime_e2e` PASSED: создание пространства с квотой → job (exit 3, раздельные
stdout/stderr) → запись/чтение/патч файла → публикация → скачивание → SHA-256 →
удаление пространства → артефакт и отзыв ссылки.

## 3. MCP-инструменты (21/21)

`server_info`, `capabilities_list`, `workspace_create/list/info/update/stop/delete`,
`exec`, `job_status`, `job_logs`, `job_cancel`, `files_list`, `file_read`,
`file_write`, `file_patch`, `file_import`, `url_import`, `artifact_publish`,
`artifact_list`, `artifact_download`.

Scopes: `workspace:read|write|delete`, `execution:run`, `files:read|write|import`,
`artifacts:read|publish`. Старые токены `workspace.read` остаются read-only.

## 4. Web UI

Реализованы разделы: Dashboard, Workspaces (создание/состояния/лимиты/квоты/
TTL/stop/start/pin/delete), Jobs (список, cancel, stdout/stderr), Files
(навигация, чтение, редактирование, upload, publish), Artifacts (ссылки,
скачивание, отзыв), Settings (runtime checks, services, capability modules).
Сборка: `tsc --noEmit && vite build`; production-бинарник отдаёт SPA и assets
(проверено HTTP 200 на `/` и `/assets/...`). Интерактивная проверка под SSO не
выполнялась (нет сессии в среде агента).

## 5. Проверки

- `cargo test --workspace --all-features --locked`: **все тесты проходят**
  (232 unit/integration + ignored приёмки).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: чисто.
- `cargo fmt --all --check`, `git diff --check`: чисто.
- Release binary: `target/release/workspace.exe` — **15 975 424 байт**, web UI
  встроен.
- Схема SQLite: 9, миграции с автоматическим `VACUUM INTO`-бэкапом.
- Диск: C: ≈17.8 ГиБ свободно; ext4 хранилище — 21 МБ занято (после очистки
  тестовых пространств); Docker images 2.46 ГБ, build cache 2.37 ГБ.

## 6. Коммиты (все подписаны, запушены в origin/main)

`9b6a9e2`, `7c02afc`, `c3da30a`, `5a182b6`, `0601711`, `2872efb`, `c0bb00c`,
`f2c6d69`, `6cf94e5`, `bbdebdc`, `ea6f9c6`.

## 7. Оставшиеся обязательные работы

1. **Egress allowlist (этап 013)** — реализован и проверен:
   `crates/workspace-egress` (статический прокси только с CONNECT/443,
   allowlist по хостам, запрет IP-литералов и непубличных адресов, DNS pinning,
   лимиты соединений), внутренняя Docker-сеть на пространство, отдельный
   контейнер-прокси, автоматическое восстановление сетей после рестарта.
   Приёмки: `scripts/egress-acceptance.sh` (direct=000, allowed=200, denied=403,
   metadata=405) и `tests/egress_e2e.rs` (job: allowed=200, denied=403,
   сеть удаляется вместе с пространством).
2. **whisper-vad** — нет доверенного источника с checksum; модуль выключен,
   создание пространства с ним отклоняется.
3. **ClipForge Speech (v0.4.0)** — спецификация доступна, upstream содержит
   реализацию; перенос и тесты внутри Workspace не выполнены.
4. **Реальный вызов из ChatGPT** — не выполнялся: в среде агента нет
   авторизованной сессии ChatGPT. Протокольная приёмка через настоящий endpoint
   пройдена.
5. **Обновление действующего экземпляра** — отложено пользователем; кроме того,
   отсутствуют `WORKSPACE_SSO_CLIENT_SECRET` и `WORKSPACE_AUTH_KEY`.
   `deploy/windows/install.ps1` готов (бэкап, установка, проверка, rollback).
6. **Перезапуск/восстановление** — логика `interrupted`, reconciliation и
   осиротевшие контейнеры реализованы и покрыты тестами; отдельный живой
   сценарий «рестарт сервера с активным workspace» ещё не зафиксирован.

## 8. Как продолжить

```powershell
# Runtime acceptance
target\release\workspace.exe --config config.toml runtime accept

# Реальные приёмки
$env:ALXPRGS_CLIPFORGE_ARCHIVE="$env:TEMP\clipforge-acceptance.zip"
cargo test -p workspace-server --test clipforge_acceptance -- --ignored --nocapture
cargo test -p workspace-server --test mcp_e2e -- --ignored --nocapture

# Обновление экземпляра (после появления секретов и разрешения)
$env:WORKSPACE_SSO_CLIENT_SECRET="…"; $env:WORKSPACE_AUTH_KEY="…"
pwsh deploy\windows\install.ps1 -Start
```

Rollback: см. вывод `install.ps1` — копия бинарника, конфигурации и БД в
`%LOCALAPPDATA%\ALXPRGS\Workspace\backups\release-<timestamp>`.
