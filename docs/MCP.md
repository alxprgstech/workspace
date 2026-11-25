# MCP Gateway — контракты и состояние

Документ описывает фактическое состояние MCP-шлюза ALXPRGS Workspace: endpoint,
авторизацию, полный каталог инструментов, механизм передачи файлов и известные
ограничения. Он дополняет §15–16 `ARCHITECTURE.md` и `SPECIFICATION`-разделы
`MVP.md`. Всё, что здесь не отмечено как реализованное, считается
нереализованным.

## 1. Endpoint и discovery

| Элемент | Значение |
| --- | --- |
| MCP endpoint | `POST/GET/DELETE https://workspace.alxprgs.tech/chatgpt/mcp` |
| Protected Resource Metadata | `GET /.well-known/oauth-protected-resource[/chatgpt/mcp]` |
| Authorization Server Metadata | `GET /.well-known/oauth-authorization-server` |
| Транспорт | Streamable HTTP (`rmcp`) |
| Ресурс | `https://workspace.alxprgs.tech/chatgpt/mcp` |

Без секции `[mcp]` шлюз отсутствует полностью (404 на все маршруты): конфигурация
объявляет аудиторию, без которой проверка токена невозможна.

## 2. Авторизация

Поддерживается локальный OAuth Authorization Server:

- Authorization Code Flow;
- PKCE S256 (обязателен);
- точные redirect URI, зарегистрированные для клиента;
- статические `client_id`/`client_secret` (CLI `workspace mcp-clients register`);
- короткоживущие access-токены и refresh-token rotation с обнаружением replay;
- отзыв доступа;
- consent-страница с перечнем запрашиваемых scopes.

Внешние JWT (RS256, issuer/audience/resource/scopes) также принимаются, если
настроен внешний IdP; локальные opaque-токены проверяются в SQLite.

### Scopes

| Scope | Что разрешает |
| --- | --- |
| `workspace:read` | чтение информации о пространствах, заданиях, файлах |
| `workspace:write` | создание и изменение пространств, запуск/остановка |
| `workspace:delete` | удаление пространств |
| `execution:run` | `exec`, отмена заданий |
| `files:read` | просмотр и чтение файлов |
| `files:write` | создание, перезапись, патчи |
| `files:import` | импорт вложений и URL |
| `artifacts:read` | список и ссылки на артефакты |
| `artifacts:publish` | публикация артефактов |

Токены, выданные до появления per-operation scopes (scope `workspace.read`),
нормализуются в `workspace:read` и **не получают** дополнительных прав. Для
write-инструментов требуется повторное согласие пользователя; миграция схемы
обновляет только `allowed_scope` клиента и не трогает выданные гранты.

## 3. Каталог инструментов

Изменяющие состояние инструменты не помечаются `readOnlyHint`; у каждого
инструмента в `_meta.securitySchemes` указан требуемый scope.

| Инструмент | Назначение | Scope | Read-only |
| --- | --- | --- | --- |
| `server_info` | сборка, runtime, ресурсы, блокеры | `workspace:read` | да |
| `capabilities_list` | каталог модулей и сервисов | `workspace:read` | да |
| `workspace_create` | создание пространства (подготовка образа — асинхронно) | `workspace:write` | нет |
| `workspace_list` | список пространств владельца | `workspace:read` | да |
| `workspace_info` | состояние, лимиты, квота, TTL | `workspace:read` | да |
| `workspace_update` | имя, TTL, закрепление, режим | `workspace:write` | нет |
| `workspace_stop` | остановка контейнера, файлы сохраняются | `workspace:write` | нет |
| `workspace_delete` | удаление пространства и файлов | `workspace:delete` | нет |
| `exec` | запуск argv внутри контейнера, возвращает Job ID | `execution:run` | нет |
| `job_status` | состояние, код возврата, длительность | `workspace:read` | да |
| `job_logs` | постраничные stdout/stderr | `workspace:read` | да |
| `job_cancel` | отмена задания | `execution:run` | нет |
| `files_list` | каталоги и файлы пространства | `files:read` | да |
| `file_read` | чтение файла (UTF-8 или base64) | `files:read` | да |
| `file_write` | атомарная запись/создание | `files:write` | нет |
| `file_patch` | строгий unified diff | `files:write` | нет |
| `file_import` | вложение ChatGPT через `openai/fileParams` | `files:import` | нет |
| `url_import` | импорт по URL из allowlist | `files:import` | нет |
| `artifact_publish` | неизменяемая копия файла | `artifacts:publish` | нет |
| `artifact_list` | список артефактов владельца | `artifacts:read` | да |
| `artifact_download` | временная ссылка на скачивание | `artifacts:read` | да |

## 4. Передача файлов

### Вложения ChatGPT (`file_import`)

Инструмент объявляет `_meta["openai/fileParams"] = ["file"]`. Хост заполняет
аргумент `file` объектом `FileRef`:

```json
{
  "download_url": "https://…",
  "file_id": "…",
  "file_name": "project.zip",
  "mime_type": "application/zip"
}
```

Сервер скачивает содержимое по HTTPS сам, не пропуская байты через контекст
модели. Проверки: только HTTPS, без credentials, публичные адреса (loopback,
private, link-local, metadata, CGNAT запрещены), DNS-адрес пинится для защиты
от rebinding, редиректы валидируются и ограничены, размер ограничен
`storage.max_import_mib`, время передачи ограничено. ZIP/TAR распаковываются в
пространство с защитой от traversal, ссылок и бомб.

### Артефакты (`artifact_download`)

`artifact_publish` создаёт неизменяемую копию файла с SHA-256 и TTL.
`artifact_download` возвращает временную ссылку вида
`https://workspace.alxprgs.tech/artifacts/<token>`; токен одноразовый по смыслу
(несколько скачиваний в пределах TTL), отзывается вместе с артефактом и не
раскрывает структуру хранилища. Ссылка живёт не дольше TTL артефакта.

`file_import` также принимает архивы: распаковка идёт в `path` (каталог) и
возвращает число записей.

## 5. Ограничения и ошибки

- Размер аргументов одного вызова ограничен настройками транспорта;
- `exec` принимает только argv (не shell-строку); вывод ограничен 1 МиБ на
  поток, лишнее помечается `*_truncated`;
- длительные операции всегда выполняются как Job: завершение MCP-вызова не
  означает успешное выполнение команды;
- ошибки возвращаются структурированно: `{ "error": "<code>", "message": "…" }`
  с кодами `invalid_arguments`, `insufficient_scope`, `runtime_not_ready`,
  `workspace_not_found`, `workspace_busy`, `capability_error`,
  `image_build_failed`, `file_conflict`, `file_too_large`, `artifact_not_found`,
  `link_invalid`.

## 6. Известные ограничения

- `whisper-vad` отсутствует в текущей сборке: нет доверенного источника с
  закреплённой контрольной суммой. Модуль отдаётся как `not_installed`, и
  создание пространства с ним отклоняется.
- Сетевой режим `allowlist` (egress proxy) не реализован; пространства
  создаются только с `network = none`, а `url_import` работает из управляющего
  процесса по административному allowlist.
- Подготовка образа асинхронна: после `workspace_create` пространство находится
  в состоянии `preparing`, команды отклоняются до готовности.
