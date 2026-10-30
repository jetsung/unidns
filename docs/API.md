# 上游平台 API 汇总

本文汇总 UniDNS 实际调用的上游 DNS 平台 OpenAPI：端点、鉴权/签名方式、使用的接口、
字段映射、能力差异与错误归一化。与 [REST API](05-api.md)（UniDNS 自身对外接口）相互独立。

资料依据：`src/providers/*.rs` 的实际调用实现（行为以代码为准）。

所有平台调用均由服务端发起，凭据以 AES-256-GCM 加密落库，不经过前端。
同步与连通性验证只调用只读接口，记录写操作才会调用平台写接口。

## 1. 平台总览

| 平台 | `provider_type` | 端点 | API 版本 | 鉴权 / 签名 | 实现文件 |
|---|---|---|---|---|---|
| 阿里云云解析 DNS | `aliyun` | `https://alidns.aliyuncs.com` | `2015-01-09` | RPC POP 签名：HMAC-SHA1 + RFC3986 | `src/providers/aliyun.rs` |
| 腾讯云 DNSPod（国内站） | `dnspod` | `https://dnspod.tencentcloudapi.com` | `2021-03-23` | TC3-HMAC-SHA256（`service=dnspod`） | `src/providers/dnspod.rs` |
| 腾讯云 DNSPod（国际站） | `dnspod_intl` | `https://dnspod.intl.tencentcloudapi.com` | `2021-03-23` | 同上（密钥体系独立） | `src/providers/dnspod.rs` |
| 腾讯云 EdgeOne（国内站） | `edgeone` | `https://teo.tencentcloudapi.com` | `2022-09-01` | TC3-HMAC-SHA256（`service=teo`） | `src/providers/edgeone.rs` |
| 腾讯云 EdgeOne（国际站） | `edgeone_intl` | `https://teo.intl.tencentcloudapi.com` | `2022-09-01` | 同上（密钥体系独立） | `src/providers/edgeone.rs` |
| CloudFlare | `cloudflare` | `https://api.cloudflare.com/client/v4` | v4 | `Authorization: Bearer <api_token>` | `src/providers/cloudflare.rs` |
| 火山引擎云解析 DNS | `volcengine` | `https://dns.volcengineapi.com` | `2018-08-01` | V4 签名：HMAC-SHA256 | `src/providers/volcengine.rs` |

调用方向：

| 场景 | 使用的上游接口 |
|---|---|
| 账户连通性验证 | 阿里云 `DescribeDomains`；DNSPod `DescribeDomainList`；EdgeOne `DescribeZones`；CloudFlare `GET /zones`；火山引擎 `ListZones` |
| 同步域名列表 | 阿里云 `DescribeDomains` + `DescribeDomainNs`；DNSPod `DescribeDomainList`；EdgeOne `DescribeZones`；CloudFlare `GET /zones`；火山引擎 `ListZones` |
| 同步解析记录 | 阿里云 `DescribeDomainRecords`；DNSPod `DescribeRecordList`；EdgeOne `DescribeDnsRecords`；CloudFlare `GET /zones/{zone_id}/dns_records`；火山引擎 `ListRecords` |
| 记录增删改 / 启停 / 备注 | 各平台对应写接口，见后续章节 |

## 2. 通用约定

### 2.1 统一记录模型与平台字段

上层统一使用 `ProviderRecord`（`src/providers/mod.rs`），由各适配器完成字段归一。
主机记录 `host` 为相对域名的部分，根记录统一为 `@`；CloudFlare 与 EdgeOne 的记录名
是 FQDN，由适配器在 `@` / 相对名与完整域名之间转换。

| 统一字段 | 阿里云 | DNSPod | EdgeOne | CloudFlare | 火山引擎 |
|---|---|---|---|---|---|
| `provider_record_id` | `RecordId` | `RecordId` | `RecordId` | `id` | `RecordID` |
| `host` | `RR` | `Name` | `Name`（FQDN，需转换） | `name`（FQDN，需转换） | `Host` |
| `rtype` | `Type` | `Type` | `Type` | `type` | `Type` |
| `value` | `Value` | `Value` | `Content` | `content` | `Value` |
| `ttl` | `TTL` | `TTL` | `TTL` | `ttl` | `TTL` |
| `line` | `Line` | `Line` | 不下发 / 不展示 | 无 | `Line` |
| `proxied` | 无 | 无 | 无 | `proxied` | 无 |
| `enabled` | `Status == "Enable"` | `Status == "ENABLE"` | `Status == "enable"` | 恒为启用 | `Enable` |
| `remark` | `Remark` | `Remark`（国内站随创建/修改；国际站经 `ModifyRecordRemark`） | 无（备注仅本地保存） | `comment` | `Remark` |
| `weight` | 无 | 无 | `Weight`（A/AAAA/CNAME） | 无 | 无 |
| `priority` | 无 | 无 | `Priority`（MX） | 无 | 无 |

域名（zone）字段映射：

| 统一字段 | 阿里云 | DNSPod | EdgeOne | CloudFlare | 火山引擎 |
|---|---|---|---|---|---|
| `name` | `DomainName` | `Name` | `ZoneName` | `name` | `ZoneName` |
| `provider_domain_id` | 空（按域名定位） | `DomainId`（记录操作按域名定位） | `ZoneId` | `id` | `ZID` |
| `status` | 无 `Status`，按 `active` 兜底 | `Status` | `Status` | `status`（仅保留 `active`） | `QueryZone.Stage` + `QueryZone.IsNSCorrect`（非 0/NS 不正确/无法确认 → `error`） |
| `remark` | `Remark` | `Remark` | 无 | 无 | `Remark` |

### 2.2 项目侧入参校验

写操作在调用平台前先做本地校验（`src/web/records.rs`），校验失败不调用上游：

- `host` 非空，根记录使用 `@`；
- 记录类型限定 `A`、`AAAA`、`CNAME`、`MX`、`TXT`、`NS`、`SRV`、`CAA`；
- `value` 非空；
- `ttl` 为 1–86400 秒；
- `weight` 非空时 -1~100，`priority` 非空时 0~50；
- 火山引擎记录备注不超过 16 个字符；
- 域名备注不超过 500 个字符（阿里云上游另有 50 字符限制）。

### 2.3 错误归一化

各平台原始错误统一映射为 `ProviderError`（`src/providers/mod.rs`），Web 层再映射为
HTTP 状态与错误码：

| `ProviderError` | 对外 HTTP | 对外错误码 |
|---|---|---|
| `Auth` | 400 | `provider_auth` |
| `NotFound` | 404 | `provider_not_found` |
| `Unsupported` | 409 | `provider_unsupported` |
| `RateLimited` | 429 | `provider_rate_limited` |
| `Network` | 502 | `provider_network` |
| `Api` | 502 | `provider_api` |

限流时若平台返回 `Retry-After`（CloudFlare），对外附带建议重试秒数。

## 3. 阿里云云解析 DNS

- **端点**：`https://alidns.aliyuncs.com`
- **版本**：`2015-01-09`
- **请求方式**：全部为 `GET`，参数放在 query string。
- **凭据**：`access_key_id` + `access_key_secret`（RAM AccessKey）。

### 3.1 签名（RPC POP，HMAC-SHA1）

公共参数：`Action`、`Format=JSON`、`Version=2015-01-09`、`AccessKeyId`、
`SignatureMethod=HMAC-SHA1`、`SignatureVersion=1.0`、`SignatureNonce`（16 字节随机 hex）、
`Timestamp`（UTC `YYYY-MM-DDTHH:MM:SSZ`）、`Signature`。

1. 参数按参数名排序，键和值分别做 RFC3986 编码（仅保留 `A-Za-z0-9-_.~`），以 `&` 连接；
2. `StringToSign = "GET&%2F&" + percentEncode(CanonicalizedQueryString)`；
3. `Signature = Base64(HMAC-SHA1(secret + "&", StringToSign))`；
4. 请求 `GET {endpoint}/?{canonical query + Signature}`。

### 3.2 使用的接口

| Action | 用途 | 主要请求参数 | 关键响应 / 说明 |
|---|---|---|---|
| `DescribeDomains` | 验证、域名列表 | `PageNumber`、`PageSize`（100） | `Domains.Domain[]`：`DomainName`、`Remark`；不返回 `Status`，统一记 `active` |
| `DescribeDomainNs` | 过滤未托管域名 | `DomainName` | `AllAliDns`：`true` 表示 NS 全部指向阿里云解析 |
| `DescribeDomainRecords` | 解析记录列表 | `DomainName`、`PageNumber`、`PageSize`（100） | `DomainRecords.Record[]`：`RecordId`、`RR`、`Type`、`Value`、`TTL`、`Line`、`Status`、`Remark` |
| `AddDomainRecord` | 创建记录 | `DomainName`、`RR`、`Type`、`Value`、`TTL`、`Line`（默认 `default`） | 返回 `RecordId`；主接口不支持备注 |
| `UpdateDomainRecord` | 修改记录 | `RecordId`、`RR`、`Type`、`Value`、`TTL`、`Line` | 主接口不支持备注 |
| `UpdateDomainRecordRemark` | 写记录备注 | `RecordId`、`Remark` | 创建后或每次修改后单独调用，空串清除 |
| `DeleteDomainRecord` | 删除记录 | `RecordId` | — |
| `SetDomainRecordStatus` | 启用 / 停用 | `RecordId`、`Status=Enable/Disable` | — |
| `UpdateDomainRemark` | 写域名备注 | `DomainName`、`Remark` | 空串清除；平台限制 50 字符 |

### 3.3 行为说明

- `list_domains` 对 `DescribeDomains` 返回的每个域名再调用 `DescribeDomainNs`，
  仅保留 `AllAliDns=true` 的域名；NS 探测失败时保守保留。
  列表接口的 `DnsServers` 是添加域名时的历史值，注册商改 NS 后不会更新，因此不采用。
- 创建记录时若带备注，创建成功后再调用 `UpdateDomainRecordRemark` 回写。
- 修改记录时无论备注是否变化，都会调用一次 `UpdateDomainRecordRemark`（空串表示清除），
  保证上游与本地备注一致。
- 分页以单页返回条数是否小于 100 判断结束。

### 3.4 错误码归一

| 平台错误码 | 归一结果 |
|---|---|
| `InvalidAccessKeyId.NotFound`、`InvalidAccessKeyId`、`SignatureDoesNotMatch`、`Forbidden`、`Unauthorized` | `Auth` |
| `Throttling`、`Throttling.User`、`Throttling.UserQos`、`FlowLimitExceeded` | `RateLimited` |
| `InvalidDomain.NotFound`、`InvalidRecord.NotFound`、`DomainNotExists` | `NotFound` |
| 其他 | `Api { code, message }` |

## 4. 腾讯云 DNSPod

- **端点**：国内站 `https://dnspod.tencentcloudapi.com`，国际站 `https://dnspod.intl.tencentcloudapi.com`
- **版本**：`2021-03-23`
- **请求方式**：`POST {endpoint}/`，JSON 请求体。
- **凭据**：`secret_id` + `secret_key`；国内站与国际站账号体系独立。

### 4.1 签名（TC3-HMAC-SHA256）

请求头：

```text
Authorization: TC3-HMAC-SHA256 Credential=<SecretId>/<YYYY-MM-DD>/dnspod/tc3_request,
               SignedHeaders=content-type;host;x-tc-action, Signature=<hex>
Content-Type: application/json; charset=utf-8
X-TC-Action: <Action>
X-TC-Timestamp: <Unix 秒>
X-TC-Version: 2021-03-23
```

规范化请求固定为：

```text
POST
/

content-type:application/json; charset=utf-8
host:<endpoint host>
x-tc-action:<action 小写>

content-type;host;x-tc-action
SHA256(body)
```

派生密钥：`HMAC(("TC3" + secret_key), date)` → `HMAC(..., service)` → `HMAC(..., "tc3_request")`，
最后对 `StringToSign` 做 HMAC-SHA256 并转 hex。

响应体为 `{"Response": {...}}`；错误位于 `Response.Error`。

### 4.2 使用的接口

| Action | 用途 | 主要请求参数 | 关键响应 / 说明 |
|---|---|---|---|
| `DescribeDomainList` | 验证、域名列表 | `Limit`（100）、`Offset` | `DomainList[]`：`Name`、`DomainId`、`Remark`、`Status`、`DNSStatus` |
| `DescribeRecordList` | 解析记录列表 | `Domain`、`Limit`（100）、`Offset` | `RecordList[]`：`RecordId`（数字）、`Name`、`Type`、`Value`、`TTL`、`Line`、`Status`、`Remark` |
| `CreateRecord` | 创建记录 | `Domain`、`SubDomain`、`RecordType`、`RecordLine`、`Value`、`TTL`、`Remark?` | 返回 `RecordId`；国际站忽略 `Remark` |
| `ModifyRecord` | 修改记录 | `Domain`、`RecordId`（数字）、`SubDomain`、`RecordType`、`RecordLine`、`Value`、`TTL`、`Remark?` | 备注随修改携带；国际站忽略 `Remark` |
| `DeleteRecord` | 删除记录 | `Domain`、`RecordId`（数字） | — |
| `ModifyRecordStatus` | 启用 / 停用 | `Domain`、`RecordId`、`Status=ENABLE/DISABLE` | — |
| `ModifyRecordRemark` | 写记录备注 | `Domain`、`RecordId`（数字）、`Remark` | 空串清除；国内站与国际站共用 |
| `ModifyDomainRemark` | 写域名备注 | `Domain`、`Remark` | 空串清除；国内站与国际站共用 |

### 4.3 行为说明

- `DNSStatus=DNSERROR` 表示 NS 未按分配指向，解析托管无效，列表同步时过滤掉；
  空值或其他值视为正常。
- 国内站默认线路为 `默认`，国际站为 `Default`（大小写敏感）。`RecordLine` 传线路名称，
  不接受 `LineId` `"0"`。
- 记录 ID 必须是数字，适配器在调用前完成转换。
- 国内站记录备注随 `CreateRecord` / `ModifyRecord` 携带，也可使用 `ModifyRecordRemark`；
  修改时空串 `Remark` 表示清除备注；域名备注使用 `ModifyDomainRemark`。
- 国际站 `CreateRecord` / `ModifyRecord` 接受 `Remark` 字段但不保存，因此创建、修改（含清空）
  之后适配器会额外调用 `ModifyRecordRemark` 回写备注（空串即清除）；域名备注使用
  `ModifyDomainRemark`。

### 4.4 错误码归一

| 平台错误码特征 | 归一结果 |
|---|---|
| 以 `AuthFailure` 或 `InvalidCredential` 开头、`UnauthorizedOperation` | `Auth` |
| 包含 `LimitExceeded` 或 `RequestLimit` | `RateLimited` |
| 以 `ResourceNotFound` 开头 | `NotFound` |
| 其他 | `Api { code, message }` |

## 5. 腾讯云 EdgeOne

- **端点**：国内站 `https://teo.tencentcloudapi.com`，国际站 `https://teo.intl.tencentcloudapi.com`
- **版本**：`2022-09-01`
- **请求方式**：`POST {endpoint}/`，JSON 请求体。
- **凭据**：`secret_id` + `secret_key`；国内站与国际站独立。
- **签名**：TC3-HMAC-SHA256，与 DNSPod 共用实现，`service=teo`，scope 为
  `<date>/teo/tc3_request`，请求头同为 `content-type`、`host`、`x-tc-action`。
- **节流**：适配器保证相邻两次调用至少间隔 100ms（EdgeOne 有频率配额限制）。

### 5.1 使用的接口

| Action | 用途 | 主要请求参数 | 关键响应 / 说明 |
|---|---|---|---|
| `DescribeZones` | 验证、站点列表 | `Limit`（200）、`Offset` | `Zones[]`：`ZoneName`、`ZoneId`、`Status`、`NSDetail`；`TotalCount` |
| `DescribeDnsRecords` | 解析记录列表 | `ZoneId`、`Limit`（1000）、`Offset` | `DnsRecords[]`：`RecordId`、`Name`（FQDN）、`Type`、`Content`、`TTL`、`Status`、`Weight`、`Priority`；`TotalCount` |
| `CreateDnsRecord` | 创建记录 | `ZoneId`、`Name`（FQDN）、`Type`、`Content`、`TTL`、`Weight?`、`Priority?` | 返回 `RecordId` |
| `ModifyDnsRecords` | 修改记录（批量） | `ZoneId`、`DnsRecords[]`（单条修改传一条） | 条目含 `RecordId`、`Name`、`Type`、`Content`、`TTL`、`Weight`、`Priority` |
| `DeleteDnsRecords` | 删除记录（批量） | `ZoneId`、`RecordIds[]` | — |
| `ModifyDnsRecordsStatus` | 启用 / 停用（批量） | `ZoneId` + `RecordsToEnable[]` 或 `RecordsToDisable[]` | 单条操作传一个元素的数组 |

### 5.2 行为说明

- 只同步 NS 接入的站点：`NSDetail` 为空的站点（CNAME 接入、EdgeOne Pages、无域名接入等）
  不托管 DNS，`DescribeDnsRecords` 不可用，直接排除。
- 系统自动站点 `default-pages-zone` 排除。
- 记录名使用 FQDN，适配器负责 `@` / 相对名与完整域名互转。
- 不下发解析线路 `Location`，统一使用平台默认线路；`line` 在统一视图中为空。
- 权重 `Weight` 仅对 `A` / `AAAA` / `CNAME` 下发，取值 -1~100；优先级 `Priority` 仅对 `MX`
  下发，取值 0~50。字段未填写时不携带，由平台默认值（权重 -1、优先级 0）处理；
  读取时 -1 / 0 归一为「未设置」。
- 无记录备注字段，备注仅保存在本地（创建/修改/同步均不丢失）；无 `proxied` 概念
  （EdgeOne 代理在站点接入层，不属于 DNS 记录）。
- 分页以原始返回条数与 `TotalCount` 判断结束，避免过滤后死循环。

### 5.3 错误码归一

| 平台错误码特征 | 归一结果 |
|---|---|
| 以 `AuthFailure` 或 `InvalidCredential` 开头、`UnauthorizedOperation` | `Auth` |
| 包含 `LimitExceeded` 或 `RequestLimit` | `RateLimited` |
| 以 `ResourceNotFound` 或 `ResourceUnavailable` 开头 | `NotFound` |
| 其他 | `Api { code, message }` |

## 6. CloudFlare

- **Base URL**：`https://api.cloudflare.com/client/v4`
- **鉴权**：`Authorization: Bearer <api_token>`，Token 需具备目标 Zone 的
  `Zone / DNS / Edit` 权限。
- **响应包裹**：`{"success": bool, "errors": [...], "result": ..., "result_info": {...}}`。
  以 `success` 判断成功；错误取 `errors[0]` 的 `code` 与 `message`。
- **分页**：使用 `page`、`per_page`，以 `result_info.total_pages` 判断结束。

### 6.1 使用的接口

| 方法与路径 | 用途 | 主要参数 | 关键响应 |
|---|---|---|---|
| `GET /zones?per_page=50&page=N` | 验证、域名列表 | `page`、`per_page` | `result[]`：`id`、`name`、`status`；仅保留 `status=active` |
| `GET /zones/{zone_id}/dns_records?per_page=100&page=N` | 解析记录列表 | `page`、`per_page` | `result[]`：`id`、`name`、`type`、`content`、`ttl`、`proxied`、`comment` |
| `POST /zones/{zone_id}/dns_records` | 创建记录 | `type`、`name`（FQDN）、`content`、`ttl`、`proxied`、`comment?` | 返回创建后的记录 |
| `PUT /zones/{zone_id}/dns_records/{record_id}` | 修改记录 | 同创建；`comment` 传空串清除 | 更新成功即采用本地视图 |
| `DELETE /zones/{zone_id}/dns_records/{record_id}` | 删除记录 | — | — |

### 6.2 行为说明

- 仅同步 `status=active` 的 Zone；`pending` 表示 NS 未切换，`moved` / `deleted` 同样排除。
- 记录名使用 FQDN，适配器负责与相对 `host` 互转；根记录为 `@`。
- 创建时 `proxied` 默认 `false`；修改使用 `PUT` 全量字段。
- 记录备注使用 `comment` 字段，随创建/修改携带；没有单独的上游备注接口。
- 平台不提供 DNS 记录级启停，调用启停返回 `Unsupported`。
- 域名无备注概念，平台回写返回 `Unsupported`，域名备注仅保存在本地。
- 读取记录时 `enabled` 恒为 `true`；`ttl` 缺省为 1。

### 6.3 错误码归一

| HTTP 状态 | 归一结果 |
|---|---|
| 401、403 | `Auth` |
| 429 | `RateLimited`（解析 `Retry-After`） |
| 404 | `NotFound` |
| 其他 | `Api { code, message }` |

## 7. 火山引擎云解析 DNS

- **端点**：`https://dns.volcengineapi.com`
- **版本**：`2018-08-01`
- **服务 / Region**：`service=dns`，`region=Global`（云解析 DNS 为 Global 服务）。
- **凭据**：`access_key_id` + `access_key_secret`（IAM 访问密钥，需云解析 DNS 权限）。
- **官方文档**：
    - [域名管理（ListZones）](https://api.volcengine.com/api-docs/view?serviceCode=dns&version=2018-08-01&action=ListZones)
    - [域名解析管理（ListRecords）](https://api.volcengine.com/api-docs/view?serviceCode=dns&version=2018-08-01&action=ListRecords)
- **请求方式**：RPC 风格。读接口用 `GET`，`Action`、`Version` 与业务参数全在 query；
  写接口用 `POST`，query 仅含 `Action`、`Version`，业务参数放 JSON body。

### 7.1 签名（V4，HMAC-SHA256）

请求头：`X-Date`（UTC `YYYYMMDDTHHMMSSZ`）、`Authorization`；POST 另带
`X-Content-Sha256`（body 的 SHA256 hex）与 `Content-Type: application/json`。

1. 规范化 query：参数按 key 升序，键值做 RFC3986 编码后以 `&` 连接；
2. 规范化请求：

   ```text
   GET  →  METHOD\n/\n<query>\nhost:<host>\nx-date:<x-date>\n\nhost;x-date\n<payload hash>
   POST →  METHOD\n/\n<query>\nhost:<host>\nx-content-sha256:<hash>\nx-date:<x-date>\n\n
           host;x-content-sha256;x-date\n<payload hash>
   ```

3. `StringToSign = "HMAC-SHA256\n" + X-Date + "\n" + scope + "\n" + SHA256(CanonicalRequest)`；
4. scope 为 `<YYYYMMDD>/Global/dns/request`；
5. 派生密钥：`kDate = HMAC(SK, date)` → `kRegion = HMAC(kDate, "Global")` →
   `kService = HMAC(kRegion, "dns")` → `kSigning = HMAC(kService, "request")`，
   每步以上一步的原始字节作为密钥；
6. `Authorization: HMAC-SHA256 Credential=<AK>/<scope>, SignedHeaders=<...>, Signature=<hex>`。

响应为 `{"ResponseMetadata": {...}, "Result": {...}}`；错误位于
`ResponseMetadata.Error`。

### 7.2 使用的接口

| Action | 方法 | 用途 | 主要请求参数 | 关键响应 / 说明 |
|---|---|---|---|---|
| `ListZones` | GET | 验证、域名列表、按域名定位 | `PageNumber`、`PageSize`（≤500）；可选 `Key`、`SearchMode=exact` | `Result.Zones[]`：`ZID`、`ZoneName`、`Remark`；`Result.Total`。响应中的 `CacheStage` 不反映当前接入状态，不用于状态判定 |
| `QueryZone` | GET | 查询域名权威状态 | `ZID` | `Result.Stage`（0 正常；1 待找回；2 未使用 TrafficRoute 解析；3 异常；5 请变更 DNS 服务器）、`Result.IsNSCorrect` |
| `ListRecords` | GET | 解析记录列表 | `ZID`、`PageNumber`、`PageSize`（500） | `Result.Records[]`：`RecordID`、`Host`、`Type`、`TTL`、`Line`、`Value`、`Weight`、`Enable`、`Remark`、`Tags` |
| `CreateRecord` | POST | 创建记录 | `ZID`、`Host`、`Type`、`Value`、`Line`（默认 `default`）、`TTL`、`Remark?` | 返回 `Result`（含 `RecordID` 等） |
| `UpdateRecord` | POST | 修改记录 | `RecordID`、`Host`、`Line`（两者必填）、`Type`、`Value`、`TTL`、`Remark?` | 修改时必须回填当前主机记录与线路 |
| `UpdateRecordStatus` | POST | 启用 / 停用 | `RecordID`、`Enable` | — |
| `DeleteRecord` | POST | 删除记录 | `RecordID` | — |
| `QueryRecord` | GET | 查询单条记录 | `RecordID` | `Result` 中含当前 `Host`、`Line`、`Type`、`Value`、`TTL`、`Remark` |
| `UpdateZone` | POST | 写域名备注 | `ZID`、`Remark` | 空串清除 |

### 7.3 行为说明

- 域名状态以 `QueryZone` 的 `Stage` 与 `IsNSCorrect` 判定：`Stage=0` 且 NS
  正确 → `enable`；`Stage` 非 0、`IsNSCorrect=false`、`Stage` 缺失或查询失败
  → `error`。`ListZones` 的 `CacheStage` 不参与判定（实测未设置 NS 时仍为 0），
  也不再将缺失状态兜底为正常。单个域名查询失败不影响其他域名同步。
- 记录启用状态由 `Enable` 布尔值映射。
- `UpdateRecord` 要求 `Host`、`Line` 必填，修改与单独更新备注时均需回填；
  只改备注时先 `QueryRecord` 取当前 `Host`/`Line`，再 `UpdateRecord`。
  备注为空串时同样下发 `Remark: ""`，用于清除平台侧备注。
- 更新域名备注时，域名无法直接得到 `ZID`：先调用
  `ListZones(Key=<域名>, SearchMode=exact)` 精确定位，再 `UpdateZone`。
- 记录备注按官方中文文档保守限制为 16 个字符，超长在本地拒绝，不调用平台。
- 权重与优先级不参与统一模型，`weight`、`priority`、`proxied` 均为空。
- 分页以单页返回条数是否小于 500 判断结束。

### 7.4 错误码归一

| 平台错误码 | 归一结果 |
|---|---|
| `InvalidAccessKey`、`SignatureDoesNotMatch`、`MissingAuthenticationToken`、`InvalidCredentials`、`ServiceNotAuthorized`、`Unauthorized` | `Auth` |
| `RequestLimitExceeded`、`Throttling`、`FlowLimitExceeded` | `RateLimited` |
| `ErrZoneNotFound`、`ErrRecordNotFound` | `NotFound` |
| 其他 | `Api { code, message }` |

## 8. 跨平台能力矩阵

| 能力 | 阿里云 | DNSPod | EdgeOne | CloudFlare | 火山引擎 |
|---|---|---|---|---|---|
| 凭据验证 | ✅ | ✅ | ✅ | ✅ | ✅ |
| 域名列表 | ✅ | ✅ | ✅（仅 NS 接入站点） | ✅（仅 active） | ✅ |
| 记录列表 | ✅ | ✅ | ✅ | ✅ | ✅ |
| 创建 / 修改 / 删除 | ✅ | ✅ | ✅ | ✅ | ✅ |
| 记录启停 | ✅ | ✅ | ✅ | ❌ 返回 `Unsupported` | ✅ |
| 记录备注（随创建/修改） | ✅（创建后单独写） | ✅ 国内站随接口，国际站随后调用 `ModifyRecordRemark` | ❌ 上游无，仅本地 | ✅ | ✅ |
| 记录备注（单独更新接口） | ✅ `UpdateDomainRecordRemark` | ✅ `ModifyRecordRemark`（国内/国际站） | ❌ 上游无 | ❌ 无单独接口 | ✅ `QueryRecord` + `UpdateRecord` |
| 域名备注 | ✅ | ✅ | ❌ 仅本地 | ❌ 仅本地 | ✅ |
| 解析线路 | ✅ | ✅ | ❌ | ❌ | ✅ |
| 权重 / 优先级 | ❌ | ❌ | ✅ | ❌ | ❌ |
| 代理状态 `proxied` | ❌ | ❌ | ❌ | ✅ | ❌ |

说明：

- 「❌ 仅本地」表示平台无该字段，`set_domain_remark` 返回 `Unsupported`，Web 层忽略该错误
  并只保存本地备注。
- 所有平台的记录表单均可填写备注。上游有记录备注字段的平台（阿里云、DNSPod 国内站、
  DNSPod 国际站、CloudFlare、火山引擎）在创建、修改、清空时回写上游，同步时以上游返回值为准
  （含平台侧清空）；EdgeOne 无有效记录备注字段，备注仅保存在本地，
  `DnsProvider::supports_record_remark` 返回 `false`，创建/修改/同步均保留本地备注。
- DNSPod 国内站与 CloudFlare 的记录备注随创建/修改接口携带，修改时空串表示清除。
- DNSPod 国际站创建、修改（含清空）后额外调用 `ModifyRecordRemark` 回写备注。
- 阿里云创建接口不带备注，创建成功后补一次 `UpdateDomainRecordRemark`。
- 火山引擎 `UpdateRecord` 修改时下发当前备注（含空串清除）；单独更新备注时先
  `QueryRecord` 回填必填字段。

## 9. 代码索引

| 内容 | 位置 |
|---|---|
| 平台类型、凭据字段、统一记录模型、`DnsProvider` 抽象 | `src/providers/mod.rs` |
| 阿里云 RPC 签名与接口调用 | `src/providers/aliyun.rs` |
| 腾讯云 TC3 签名（DNSPod / EdgeOne 共用） | `src/providers/dnspod.rs` |
| EdgeOne 接口与权重/优先级规则 | `src/providers/edgeone.rs` |
| CloudFlare v4 调用与响应解析 | `src/providers/cloudflare.rs` |
| 火山引擎 V4 签名与接口调用 | `src/providers/volcengine.rs` |
| 记录入参校验与回写流程 | `src/web/records.rs` |
| 同步流程（只读接口、限流重试） | `src/sync.rs` |
