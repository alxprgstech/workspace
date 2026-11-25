# ALXPRGS Workspace — отчёт о состоянии (не итоговый COMPLETION)

> **Статус: проект НЕ завершён.** Документ фиксирует фактическое состояние на
> момент последнего раунда разработки, доказательства и точный перечень
> оставшихся работ. Он намеренно не называется `COMPLETION_REPORT.md`, потому что
> критерии завершения (раздел 17 задания) пока не выполнены.

## 1. Итоговый статус

| Этап | Состояние | Доказательство |
| --- | --- | --- |
| 001–007 (база, SSO, OAuth, MCP transport) | завершён | тесты, реальное подключение ChatGPT (ранее) |
| 008 Runtime Backend | **завершён** | `runtime accept` PASSED на выделенном WSL2 |
| 009 Workspace Manager | **завершён** | реальный E2E: создание/квоты/остановка/удаление |
| 010 Capability Images | **реализован, проверен сборкой** | образ собран и проверен: cargo/rustc/clippy/g++/Python/ffmpeg |
| 011 Execution & Jobs | **завершён** | реальный job: exit code, stdout/stderr, отмена, idempotency |
| 012 Files & Artifacts | **реализован** (кроме финального E2E импорта) | реальный E2E: write/read/patch/publish/download+SHA-256 |
| 013 Security & Isolation | частично | изоляция и лимиты подтверждены; egress allowlist не реализован |
| 014 Финальная приёмка | частично | runtime E2E, файлы/артефакты; ClipForge — см. §7 |
| Web UI | **не завершён** | дашборд работает; разделы Workspaces/Jobs/Files/Artifacts отключены |

## 2. Что реализовано и проверено практически

### 2.1 Runtime (008)

- Конфигурируемый backend `wsl2` / `rootless_docker`, явное указание
  дистрибутива, пользователя, socket, имени engine, data root, helper, лимитов.
- Проверки: engine reachable/dedicated/rootless/cgroup v2/seccomp, WSL config
  (interop/automount/appendWindowsPath), ext4+prjquota, свободное место, helper
  (протокол, uid≠0).
- Практическая приёмка запускает реальный контейнер и проверяет: CapEff=0,
  NoNewPrivs=1, Seccomp=2, read-only rootfs, отсутствие docker.sock, только
  loopback, отсутствие маршрутов, memory/swap/pids/cpu limits, uid_map `0 1000 1`.
- Реальное исчерпание блочной и inode-квоты на ext4 project quota.
- Холодный старт WSL2, таймауты, kill_on_drop, единый `RuntimeStatus` для
  doctor/`/ready`/HTTP/MCP/CLI.

Вывод `workspace runtime accept`: **PASSED**.

### 2.2 Workspaces (009)

- SQLite-схема v8 (миграции 0005–0008) с автоматическим `VACUUM INTO`-бэкапом
  перед миграцией.
- Состояния, TTL/закрепление, idle timeout, лимит активных пространств, квоты,
  reaper, reconciliation осиротевших контейнеров, идемпотентная подготовка.
- Асинхронная подготовка образа: `workspace_create` возвращает `preparing`,
  команды до готовности отклоняются.
- HTTP API `/api/v1/workspaces…`, MCP-инструменты.

### 2.3 Capability Images (010)

- Доверенные рецепты закомпилированы в бинарник (`images/manifests.toml`),
  MCP-клиент передаёт только идентификаторы модулей.
- Канонический cache key, таблица `image_specs`, ограниченная сборка
  (2 ГиБ RAM, 2 CPU, 30 мин), пиннинг по immutable image ID в теге,
  последовательные сборки, повторное использование образа.
- `rust` использует закреплённый `rust:1.100-slim`; остальные модули — apt на
  закреплённом Debian digest.
- Собранный образ проверен: `cargo 1.100.0`, `rustc 1.100.0`,
  `clippy 0.1.100`, `g++ 14.2.0`, `Python 3.13.5`, `ffmpeg 7.1.5`.
- `whisper-vad` **отсутствует**: нет доверенного источника с закреплённой
  контрольной суммой; модуль отдаётся как `not_installed`, создание пространства
  с ним отклоняется.

### 2.4 Jobs (011)

- `exec/job_status/job_logs/job_cancel`, argv без shell, cwd, env, таймауты,
  отмена, idempotency key, один активный job на пространство, глобальный
  семафор, bounded stdout/stderr (1 МиБ на поток), `interrupted` при рестарте.
- Реально проверено: job в контейнере, exit code 3, раздельные stdout/stderr,
  повтор с тем же idempotency key не создаёт дубликат.

### 2.5 Files & Artifacts (012)

- `files_list/file_read/file_write/file_patch`, атомарные записи, строгий
  unified diff (включая CRLF), проверка владельца.
- `file_import` (документированный `_meta["openai/fileParams"]`), `url_import`
  (allowlist + SSRF-защита: только HTTPS, публичные адреса, pin DNS, редиректы,
  лимиты), безопасная распаковка ZIP/TAR (traversal, ссылки, бомбы, лимиты,
  сохранение exec-бита).
- `artifact_publish/list/download`: неизменяемая копия, SHA-256, TTL, временные
  отзываемые ссылки `/artifacts/{token}`, артефакт переживает удаление
  пространства.
- Реально проверено в E2E: запись/чтение/патч, публикация, скачивание,
  SHA-256 совпал, ссылка отозвана.

## 3. MCP-инструменты (21/21)

`server_info`, `capabilities_list`, `workspace_create/list/info/update/stop/delete`,
`exec`, `job_status`, `job_logs`, `job_cancel`, `files_list`, `file_read`,
`file_write`, `file_patch`, `file_import`, `url_import`, `artifact_publish`,
`artifact_list`, `artifact_download`.

Scopes: `workspace:read|write|delete`, `execution:run`, `files:read|write|import`,
`artifacts:read|publish`. Токены со старым scope `workspace.read` остаются
read-only (`workspace:read`), write-инструменты требуют повторного согласия.

## 4. Тесты и проверки

- `cargo test --workspace --all-features --locked`: **232 теста, все проходят**
  (плюс ignored-тесты реального runtime).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: чисто.
- `cargo fmt --all --check`: чисто; `git diff --check`: чисто.
- Реальные (ignored) приёмочные тесты:
  - `runtime_e2e` — PASSED;
  - `image_build` (пакетный образ и Rust-образ) — PASSED;
  - `clipforge_acceptance` — см. §7.

## 5. Версии и артефакты

| Компонент | Версия |
| --- | --- |
| Приложение | 0.1.0 |
| Схема SQLite | 8 |
| Rust | 1.100.0 |
| Docker Engine | 29.10.1 (rootless) |
| WSL kernel | 6.18.33.2-2 |
| Debian base image | `debian@sha256:8aa5a7bf…44a0` |
| Release binary | `target/release/workspace.exe`, 15 939 072 байт (с web UI) |

## 6. Git

Подписанные коммиты (`git commit -S`, ED25519), запушены в `origin/main`:

- `9b6a9e2` feat(runtime): complete the dedicated runtime backend…
- `7c02afc` feat(workspaces): add workspace manager, job executor and scoped OAuth grants
- `c3da30a` feat(mcp): expose workspace and job tools with scoped authorization
- `5a182b6` feat(files): add file service, artifacts and their MCP tools
- `0601711` test(files): prove files, artifacts and sha256 on the real runtime
- `2872efb` feat(images): build trusted capability images and add import tools
- `c0bb00c` fix(runtime): stabilise uploads, CRLF patches and rustup in containers
- `f2c6d69` fix(files): preserve executable bits when extracting archives

## 7. ClipForge acceptance — фактический результат

Подготовлен архив ClipForge на базовом коммите
`91db3f530b8f2eb7a24d35c087e18e72a6507d38` (Tauri-член исключён из workspace,
зависимости CLI завендорены — сборка идёт offline при `network = none`).

Внутри Workspace подтверждено:

- импорт архива: 3086 записей, 62 036 117 байт;
- просмотр структуры проекта и чтение `cli/Cargo.toml`;
- изменение версии CLI `0.3.0` → `0.4.0` через `file_patch` и обновление
  ожидания в `cli/tests/cli.rs`;
- `cargo check --offline -p clipforge-cli` — **успешно** (13.9 с, образ с Rust);
- запуск `cargo test` — тестовый harness выполняется; интеграционные тесты
  проекта требуют внешних медиа-инструментов (`whisper-cli`) и падают по
  причинам окружения проекта, поэтому приёмка ограничена библиотечными тестами;
- `cargo build --release` и публикация бинарника — выполнялись, но раунд
  закончился до фиксации результата.

Таким образом, Workspace подтверждённо выполняет разработку Rust-проекта
(импорт, редактирование, cargo check/test), но полный сценарий ClipForge
(сборка, артефакт, проверка скачивания) и реализация инструмента Speech
(≈1344 строки в upstream) **не завершены**.

## 8. Известные ограничения и блокеры

1. **Web UI**: работает только Dashboard; разделы Workspaces/Jobs/Files/
   Artifacts/Settings отключены. Требуется реализация (этап 11).
2. **whisper-vad**: нет доверенного, checksum-pinned источника; модуль выключен.
3. **Сетевой egress allowlist** (этап 013): не реализован; пространства
   работают только с `network = none`, импорт — через управляющий процесс.
4. **ClipForge Speech**: спецификация доступна, upstream содержит реализацию
   v0.4.0 (diff ~1344 строки), но перенос в Workspace не выполнен.
5. **Обновление действующего экземпляра**: отложено по решению пользователя;
   кроме того, в текущем окружении отсутствуют `WORKSPACE_SSO_CLIENT_SECRET` и
   `WORKSPACE_AUTH_KEY`. Скрипт обновления и rollback готовы
   (`deploy/windows/install.ps1`).
6. **Финальная проверка ChatGPT**: реальный вызов из ChatGPT в этом раунде не
   выполнялся (нет авторизованной сессии в среде агента); протокольные тесты
   MCP/OAuth проходят.

## 9. Как продолжить

```powershell
# 1. Runtime acceptance
target\release\workspace.exe --config config.toml runtime accept

# 2. ClipForge acceptance (архив подготовлен)
$env:ALXPRGS_CLIPFORGE_ARCHIVE="$env:TEMP\clipforge-acceptance.zip"
cargo test -p workspace-server --test clipforge_acceptance -- --ignored --nocapture

# 3. Обновление действующего экземпляра (после появления секретов)
$env:WORKSPACE_SSO_CLIENT_SECRET="…"; $env:WORKSPACE_AUTH_KEY="…"
pwsh deploy\windows\install.ps1 -Start
```

Rollback: см. вывод `install.ps1` (копия бинарника, конфигурации и БД в
`%LOCALAPPDATA%\ALXPRGS\Workspace\backups\release-<timestamp>`).

## 10. Безопасность: что подтверждено, а что нет

Подтверждено техническими проверками: непривилегированный контейнер (rootless
userns), отсутствие capabilities, seccomp, NoNewPrivs, read-only rootfs,
ограниченный tmpfs, CPU/RAM/swap/PID лимиты, отсутствие docker.sock и сети,
жёсткие project quotas, изоляция владельцев, scoped OAuth, SSRF-защита импорта,
безопасная распаковка.

Не подтверждено/не реализовано: контролируемый egress allowlist и защита от
обхода сетевой политики; WSL2 + rootless Docker не являются границей,
эквивалентной VM — остаточные риски описаны в `docs/MCP.md` и настоящем отчёте.
