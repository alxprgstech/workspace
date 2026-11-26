# ALXPRGS Workspace — отчёт о завершении

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
