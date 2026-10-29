# REST API

!!! info "机器可读契约"

    本页以人工阅读为主；完整的机器可读 OpenAPI 3.0 契约见 [`openapi.yaml`](./openapi.yaml)，
    可直接导入 Swagger UI / Postman 等工具链消费。接口行为以源码为准，OpenAPI 供参考。

所有接口挂在 `/api/v1` 前缀下，除登录与初始化引导外均需会话 Cookie 鉴权
（未登录返回 401）。响应错误体统一为：

```json
{"error": {"code": "not_found", "message": "资源不存在"}}
```

错误码与 HTTP 状态码映射：`invalid_request` 400、`unauthorized` 401、
`not_found` 404、`provider_auth` 400（凭据无效）、`provider_not_found` 404
（平台侧资源不存在）、`provider_unsupported` 409、`provider_rate_limited` 429、
`provider_network` / `provider_api` 502、`internal` 500。平台错误（
`provider_*`）会透传平台原始消息，限流时附带建议重试秒数。

## 认证（/auth）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/auth/status` | 初始化状态：是否已设置管理员口令 |
| POST | `/auth/setup` | 首次设置管理员口令（仅口令未设置时可用） |
| POST | `/auth/login` | 登录，设置会话 Cookie |
| GET | `/auth/me` | 当前登录信息 |
| POST | `/auth/logout` | 退出登录 |
| POST | `/auth/password` | 修改口令（需验证旧口令） |

```bash
# 登录并保持会话
curl -c cookie.txt -X POST http://127.0.0.1:18686/api/v1/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"password": "your-password"}'
```

## 账户（/accounts）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/accounts` | 账户列表（含最近同步结果） |
| POST | `/accounts` | 新建账户，需校验凭据后入库（加密存储） |
| GET | `/accounts/{id}` | 账户详情 |
| PATCH | `/accounts/{id}` | 更新名称/状态/凭据（换凭据后原验证结果作废） |
| DELETE | `/accounts/{id}` | 删除账户（级联删除其域名与记录） |
| POST | `/accounts/{id}/verify` | 调平台 API 验证凭据连通性 |
| POST | `/accounts/{id}/sync` | 单账户同步 |

```bash
curl -b cookie.txt -X POST http://127.0.0.1:18686/api/v1/accounts \
  -H 'Content-Type: application/json' \
  -d '{"name": "主账户", "provider_type": "aliyun",
       "credentials": {"access_key_id": "LTAI...", "access_key_secret": "..."}}'
```

`provider_type` 取值：`aliyun` / `dnspod` / `dnspod_intl` / `edgeone` /
`edgeone_intl` / `cloudflare` / `volcengine`。状态仅 `enabled` / `disabled`。

## 域名与记录

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/domains?page=&page_size=&search=&account_id=` | 分页域名列表（含账户名与平台名） |
| GET | `/domains/{id}` | 域名详情 |
| PATCH | `/domains/{id}` | 更新本地备注（`remark`，不回写平台） |
| GET | `/domains/{id}/records?...` | 记录列表（支持搜索/筛选，分页同上） |
| POST | `/domains/{id}/records` | 新建记录（回写上游平台，TTL/线路受平台约束） |
| PATCH | `/records/{id}` | 更新记录（host/rtype/value/ttl/line/remark/proxied/weight/priority） |
| POST | `/records/{id}/status` | 启用/停用（回写平台） |
| DELETE | `/records/{id}` | 删除记录（回写平台） |

## 同步（/sync）

| 方法 | 路径 | 说明 |
|---|---|---|
| POST | `/sync` | 触发全量同步（所有启用账户，异步启动） |
| POST | `/accounts/{id}/sync` | 触发单账户同步 |
| GET | `/sync/runs?page=` | 运行历史（分页，倒序） |
| GET | `/sync/runs/{id}` | 单次运行详情（状态/统计/错误） |
| GET | `/sync/settings` | 同步设置（间隔/开关） |
| PUT | `/sync/settings` | 更新同步设置 |

!!! note "幂等与互斥"

    同一账户同步进行中时重复触发会被拦截；全量同步期间部分账户失败不影响
    其余，结果按账户聚合在运行记录的统计里。
