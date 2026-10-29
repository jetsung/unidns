# DNS 提供商

UniDNS 内置七个平台适配器，同一平台可添加多个账户。凭据在「账户管理」里
以 JSON 形式录入，保存前会做必填字段校验并调用平台 API 做连通性验证；
凭据以 AES-256-GCM 加密落库，界面仅展示首个必填字段的打码串（如 `****9999`）。

## 凭据格式

| 平台（provider_type） | 凭据字段 | 凭据来源 |
|---|---|---|
| 阿里云（`aliyun`） | `access_key_id`、`access_key_secret` | RAM 访问控制台 → AccessKey |
| DNSPod 国内（`dnspod`） | `secret_id`、`secret_key` | 腾讯云控制台 → API 密钥 |
| DNSPod 国际（`dnspod_intl`） | `secret_id`、`secret_key` | DNSPod 国际站 → API 密钥 |
| EdgeOne 国内（`edgeone`） | `secret_id`、`secret_key` | 腾讯云控制台 → API 密钥 |
| EdgeOne 国际（`edgeone_intl`） | `secret_id`、`secret_key` | EdgeOne 国际站 → API 密钥 |
| CloudFlare（`cloudflare`） | `api_token` | My Profile → API Tokens（需 Zone.DNS 编辑权限） |
| 火山引擎（`volcengine`） | `access_key_id`、`access_key_secret` | IAM 控制台 → 访问密钥（建议为子用户创建，授予云解析 DNS 权限） |

=== "阿里云"

    ```json
    {"access_key_id": "LTAI...", "access_key_secret": "..."}
    ```

=== "DNSPod / EdgeOne（国内与国际同格式）"

    ```json
    {"secret_id": "AKID...", "secret_key": "..."}
    ```

    国内站与国际站账号体系独立，密钥不通用——请按账户实际所属选择
    `dnspod` / `dnspod_intl`、`edgeone` / `edgeone_intl`。

=== "CloudFlare"

    ```json
    {"api_token": "..."}
    ```

    使用 API Token（非 Global API Key），Token 需对目标 Zone 有
    `Zone / DNS / Edit` 权限。

=== "火山引擎"

    ```json
    {"access_key_id": "AKLT...", "access_key_secret": "..."}
    ```

    使用 IAM 访问密钥，仅要求云解析 DNS（DNS）读写权限。

## 签名方式

| 平台 | 签名 |
|---|---|
| 阿里云 | RPC 签名，HMAC-SHA1 + PercentEncode（`GET` 公共参数） |
| DNSPod / EdgeOne | 腾讯云 TC3-HMAC-SHA256（两适配器共用实现） |
| CloudFlare | `Authorization: Bearer <token>` |
| 火山引擎 | V4 签名，HMAC-SHA256（`X-Date` + `Authorization`，POST 另带 `X-Content-Sha256`；region 固定 `cn-beijing`） |

## 能力差异

所有平台均支持：验证凭据、域名列表、记录增删改查、启用/停用、记录与域名备注。
个别平台的备注能力受上游 API 限制，调用不支持的平台会返回
`ProviderError::Unsupported`（HTTP 409，错误码 `provider_unsupported`）。

## 权限最小化建议

- 阿里云：为 UniDNS 创建独立 RAM 子账户，仅授权目标域名所在
  `AliyunDNSFullAccess`（或只读 + 写入所需的最小策略）。
- 腾讯云系列：子账户仅绑定 DNSPod / EdgeOne 的读写策略。
- CloudFlare：Token 限定到具体 Zone，不使用 Global API Key。
- 火山引擎：为 UniDNS 创建独立 IAM 子用户，仅授予云解析 DNS 读写策略。

!!! tip "验证失败排查"

    「验证」按钮直接调用平台 API（阿里云 `DescribeDomains`、DNSPod/EdgeOne
    `DescribeZoneList`、CloudFlare `GET /zones`、火山引擎 `ListZones`）。失败提示透传平台原始错误
    码与消息（错误码 `provider_auth` / `provider_api`），按提示检查密钥
    是否有效、所属站点（国内/国际）是否选对、IP 是否被平台风控拦截。
