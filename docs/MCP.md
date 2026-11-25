# MCP Gateway — контракты и состояние (этап 007)

Документ фиксирует состояние MCP-шлюза ALXPRGS Workspace, уже реализованные
контракты и точный перечень незавершённых работ. Он дополняет §15–16
`ARCHITECTURE.md`; всё, что не описано здесь как реализованное, считается
нереализованным.

## 1. Состояние

| Элемент | Состояние |
| --- | --- |
| Конфигурация `[mcp]` | реализовано |
| Protected Resource Metadata (RFC 9728) | реализовано |
| Проверка MCP access token (RS256, issuer, audience, resource, срок, scopes) | реализовано |
| Отключение шлюза без конфигурации (fail-closed) | реализовано |
| Streamable HTTP транспорт (`rmcp`) | не реализовано |
| Инструменты `server_info`, `capabilities_list` | не реализовано (контракты ниже) |
| OAuth-сервер: authorization code + PKCE, DCR, refresh, revoke | не реализовано |
| JWKS-кеш с обновлением при ротации ключей для MCP | не реализовано |

Пока транспорт не готов, `POST /chatgpt/mcp` и `GET /chatgpt/mcp` отвечают
`501 mcp_transport_pending`. Незащищённый MCP-endpoint не публикуется.

## 2. Конфигурация

```toml
[mcp]
issuer = "https://auth.alxprgs.tech"                 # IdP, выпускающий токены
audience = "https://workspace.example.com/chatgpt/mcp"  # aud access token
resource = "https://workspace.example.com/chatgpt/mcp"  # RFC 8707 resource
required_scopes = ["workspace.read"]                  # обязательные scopes
```

Без секции `[mcp]` шлюз отсутствует полностью: `/.well-known/*` и
`/chatgpt/mcp` возвращают `404`. Это осознанное решение: конфигурация
объявляет аудиторию, без которой проверка токена невозможна.

## 3. Discovery

- `GET /.well-known/oauth-protected-resource`
- `GET /.well-known/oauth-protected-resource/chatgpt/mcp`

Ответ:

```json
{
  "resource": "https://workspace.example.com/chatgpt/mcp",
  "authorization_servers": ["https://auth.alxprgs.tech"],
  "scopes_supported": ["workspace.read"],
  "bearer_methods_supported": ["header"],
  "resource_name": "ALXPRGS Workspace"
}
```

## 4. Проверка access token

Реализовано в `crates/workspace-server/src/mcp_auth.rs`.

Обязательные условия приёма запроса:

1. JWT из трёх частей, `alg = RS256`; `alg: none` и любые другие алгоритмы
   отклоняются (`UnsupportedAlgorithm`).
2. Подпись проверяется ключом из JWKS по `kid`; при отсутствии `kid` JWKS
   должен содержать ровно один пригодный RSA-ключ.
3. `iss` строго равен настроенному `issuer`.
4. `aud` содержит настроенный `audience` (строка или массив).
5. Если токен содержит `resource`, он обязан совпасть с настроенным
   `resource` (RFC 8707).
6. `exp > now - 60`, `nbf <= now + 60`.
7. `scp`/`scope` содержит все `required_scopes`.
8. `sub` непустой — это идентификатор вызывающего.

Криптографические примитивы не реализуются самостоятельно: подпись
проверяется `ring` (`RSA_PKCS1_2048_8192_SHA256`), JWKS разбирается штатным
`serde_json`, base64url — крейтом `base64`.

Токены MCP не связаны с браузерными сессиями: отдельный путь проверки,
отдельная аудитория, отзыв браузерной сессии не продлевает и не отзывает
MCP-токен (отзыв появится вместе с OAuth-сервером).

## 5. Инструменты (контракты, ещё не реализованы)

`server_info` — read-only, без аргументов:

```json
{
  "product": "ALXPRGS Workspace",
  "version": "0.1.0",
  "api_version": "v1",
  "platform": "windows",
  "architecture": "x86_64",
  "schema_version": 3,
  "runtime_ready": false
}
```

`capabilities_list` — read-only, без аргументов; каталог модулей с честным
состоянием (реализовано/не реализовано), версиями и зависимостями:

```json
{
  "capabilities": [
    {"id": "web_session", "status": "ready", "version": "1"},
    {"id": "mcp_gateway", "status": "partial", "version": "1"},
    {"id": "docker_runtime", "status": "absent", "version": null}
  ]
}
```

Аннотации инструментов должны соответствовать поведению: изменяющие
состояние операции не помечаются `readOnlyHint`.

## 6. Что нужно сделать для завершения этапа 007

1. Подключить `rmcp` (Streamable HTTP, `server` + `transport-streamable-http-server`)
   и смонтировать сервис в `/chatgpt/mcp` за middleware проверки Bearer-токена
   из §4; сейчас зависимость не подключена.
2. Реализовать `server_info` и `capabilities_list` по контрактам §5,
   с интеграционными тестами `initialize` → `tools/list` → `tools/call`.
3. Добавить JWKS-кеш с TTL и принудительным обновлением при неизвестном `kid`.
4. Реализовать OAuth-сервер: authorization code + PKCE S256, metadata,
   refresh token rotation, отзыв, точные redirect URI, статические clients.
5. Ограничить размеры запросов и ответов, добавить структурированные ошибки.
6. Проверить подключение из обычного веб-чата ChatGPT на целевом аккаунте;
   до этого этапа NGINX Basic Auth остаётся включённой.

## 7. Безопасность

- Шлюз не публикует endpoint без объявленной аудитории.
- Токены принимаются только в заголовке `Authorization: Bearer`.
- Секреты SSO (`client_secret`, ключи шифрования) не используются для
  проверки MCP-токенов и не логируются.
- Запрещено включать MCP-endpoint в интернете до появления полноценной
  проверки токена, отзыва и ограничения размеров.
