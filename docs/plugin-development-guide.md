# Agent2API 插件开发流程（参考实现：Today AI）

本文把 `crates/providers-todayai` 从零到「装进网关、账号可加、模型可调」的全过程
整理成可复用的步骤，供后续开发其他 provider 时对照。

参考实现是**独立包**（只含一家）。合集包（`crates/providers`，二十多家一起编译）
的差别只在「打包入口」和「多个 provider 注册进同一个二进制」，其余完全一致。

---

## 0. 先理解三个平面

一个 provider 插件本质上是**一个独立进程 + 一份清单 + 可选的一个网页**。网关启动它，
通过 stdio 上的 IPC 调它的 `Provider` trait 方法。

```text
         ┌──────────── 网关（agent2api.exe）────────────┐
         │  管理 API :3065    OpenAI API /v1/...        │
         └───────┬───────────────────────┬─────────────┘
                 │ 管理面（账号 / 目录）  │ 数据面（chat）
                 │  control RPC          │  stream RPC
         ┌───────▼───────────────────────▼─────────────┐
         │  插件进程（你的二进制，SDK 的 serve 主循环）  │
         └───────┬───────────────────────┬─────────────┘
                 │                       │
         ┌───────▼────────┐      ┌───────▼──────────────┐
         │ 账号面（登录） │      │ 数据面（推理转发）    │
         │ 发码 / 换令牌  │      │ 无状态 or 有状态      │
         └────────────────┘      └──────────────────────┘
```

三个平面各有自己的入口方法，**互不替代**：

| 平面 | 插件方法 | 网关侧消费方 |
|---|---|---|
| 账号 | `normalize_account` / `account_view` / `ensure_token` / `refresh_token` | 账号页、路由选路、凭证维护 |
| 目录 | `models` / `refresh_models` | 「刷新模型清单」、`/v1/models` |
| 数据面 | `build_chat`（无状态）或 `conversation`（有状态） | `/v1/chat/completions` |

---

## 1. 动手前必须先回答的五个问题

这五个答案决定了整个骨架，事后改代价很大。

| 问题 | 影响 |
|---|---|
| 上游有 OpenAI 兼容的推理接口吗？ | 决定 `stateful`，见 §6 |
| 用户怎么登录？ | 手机验证码 / 扫码轮询 / 桌面端导入，决定 `login_*` 与账号页 |
| 有没有签到、余额、领取？ | 决定 `checkin` / `usage` / `claims`，**没有就不要申报** |
| 模型目录从哪来、什么形状？ | 决定 `models.rs` 的解析与缓存 |
| 单独发包还是进合集？ | 决定目录、`pack.json`、CI job |

> 判定「有没有 OpenAI 兼容接口」不要靠猜：先用真实令牌把候选路径全打一遍
> （见 §10.1），再决定架构。Today AI 就是先按 OpenAI 兼容写完，实测全 404，
> 最后整体改成有状态。

---

## 2. 目录骨架

```text
crates/providers-<name>/
├── Cargo.toml
├── pack.json                 # 插件清单（网关按它安装 / 启动 / 挂页面）
├── src/
│   ├── main.rs               # 入口：注册 provider，交给 serve_blocking
│   ├── http.rs               # 出网薄封装（代理 + 超时 + JSON 收发）
│   ├── store.rs              # secret 写回辅助（账号库在网关侧）
│   └── <name>/
│       ├── mod.rs            # Provider 实现：meta + 数据面 + 目录 + 账号
│       ├── endpoints.rs      # 上游基址 / 客户端标识头（环境变量可覆盖）
│       ├── client.rs         # 请求头组装 + 错误翻译
│       ├── login.rs          # 登录协议（发码 / 换令牌）
│       ├── credentials.rs    # secret 结构、令牌换发、账号视图
│       ├── models.rs         # 目录拉取 + 解析 + 缓存
│       └── chat.rs           # 数据面（无状态可省；有状态必须）
└── ui/<name>/                # 可选：插件自带页面
    ├── index.html
    ├── app.js
    ├── api.js                # 页面 ↔ 网关管理 API 的最小客户端
    └── page.css
```

**为什么 `http.rs` / `store.rs` 各留一份**：合集包里的同名模块是那个 crate 的私有模块，
不能跨 crate 复用；把它们提进 `provider-sdk` 会扩大第三方插件也要编译的契约面。
用量很小时各留一份更便宜。

---

## 3. 建 crate 并注册 provider

### 3.1 `Cargo.toml`

```toml
[package]
name = "agent2api-providers-<name>"
description = "Agent2API <Name> 提供商插件包（独立发布）"
version = "0.1.0"                 # 每次重打包都必须 bump，见 §8.3
edition.workspace = true
rust-version.workspace = true
license-file.workspace = true
repository.workspace = true
homepage.workspace = true

[[bin]]
name = "agent2api-<name>"         # 与 pack.json 的 entry 文件名一致
path = "src/main.rs"

[dependencies]
agent2api-provider-protocol = { path = "../provider-protocol" }
agent2api-provider-sdk = { path = "../provider-sdk" }
serde_json = "1"
reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls", "socks"] }
# 只有需要账号级串行锁时才加
tokio = { version = "1", features = ["sync"] }
```

要点：

- **`rustls` 而不是系统 openssl**：插件要在三平台上跑，不能依赖宿主装了什么。
- 只加真正用到的依赖。合集包的依赖树已经很大，独立包的意义之一就是小。
- `[[bin]]` 的名字要和 `pack.json` 的 `entry` 对得上。

### 3.2 `src/main.rs`

```rust
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod http;
mod store;
mod <name>;

use agent2api_provider_sdk::BoxProvider;

fn main() {
    let providers: Vec<BoxProvider> = vec![Box::new(<name>::<Name>Provider)];
    agent2api_provider_sdk::serve_blocking(providers);
}
```

`#![deny(...)]` 不是形式主义：release 是 `panic=abort`，一次 `unwrap` 就是整个插件
进程消失，网关只看到「插件在给出响应头之前退出」。**全部用 `?` / `ok_or_else`。**

---

## 4. `pack.json` 与 `meta()`

### 4.1 `pack.json`

```json
{
  "id": "agent2api-<name>",
  "name": "<Name> 提供商",
  "description": "……（一句话说清能力与边界）",
  "license": "AGPL-3.0",
  "version": "0.1.0",
  "protocol": 1,
  "providers": [
    { "id": "<name>", "label": "<Name>", "order": 0, "default_enabled": true }
  ],
  "ui": [
    {
      "id": "<name>",
      "title": "<Name> 账号",
      "entry": "<name>/index.html",
      "group": "g-overview",
      "order": 10
    }
  ],
  "entry": {
    "x86_64-pc-windows-msvc": "bin/agent2api-<name>.exe",
    "x86_64-unknown-linux-gnu": "bin/agent2api-<name>",
    "aarch64-unknown-linux-gnu": "bin/agent2api-<name>",
    "x86_64-apple-darwin": "bin/agent2api-<name>",
    "aarch64-apple-darwin": "bin/agent2api-<name>"
  },
  "homepage": "https://github.com/ProvidersPlugins/Agent2Providers"
}
```

要点：

- `providers[].id` 是**落盘契约**：账号记录按它存。改名 = 用户账号全丢。
- `entry` 的键是**完整 Rust 三元组**，四个消费方逐字比对（`HostEnv.target`、
  `pack.json` 的键、签名载荷 `target=`、更新资产名 `…-<target>.zip`）。
- `ui[].entry` 相对包内的 `ui/` 目录解析。
- **文件必须无 BOM**。PowerShell 的 `Set-Content` 会加 BOM，`pack.mjs` 的
  `JSON.parse` 会直接失败。用编辑器或 `[IO.File]::WriteAllText`。

### 4.2 `meta()`

```rust
fn meta(&self) -> ProviderMeta {
    ProviderMeta::new(ID, "Label")
        .with_capabilities(proto::Capabilities { /* 见下表 */ })
        .with_login(vec![proto::LoginMethod::Sms { hint: Some("…".into()) }])
        .with_models(vec![])                       // 无内置兜底就留空
        .with_account_features(proto::AccountFeatures { /* 见下表 */ })
}
```

### 4.3 `Capabilities` 决策表

| 字段 | 什么时候 true | 填错的后果 |
|---|---|---|
| `chat` | 能转发推理 | 无转发能力的家排进队首，顶栏显示成「当前登录态」 |
| `stateful` | 上游是多步会话协议 | 有状态家被当无状态，请求发出去但语义全错 |
| `refresh` | 能主动续期凭证 | 每轮白跑 + 灌失败日志 |
| `model_refresh` | 有远程目录可拉 | 用户点「刷新」永远失败 |
| `usage` | 有真实余额/积分接口 | 给了入口但必然报错 |
| `checkin` | 有每日签到或等价动作 | 一个点了必然报错的复选框 |
| `claims` | 有可领取活动 | 同上 |
| `web_login` | 能拉起官方登录页 | 打开一个换不到凭证的窗口 |
| `sse_model_rewrite` | 上游回内部模型名 | 客户端看到的模型名与请求的不一致 |
| `sse_strip_newline_chunks` | 上游用纯换行做保活 | 把模型真写的换行丢了 |
| `anonymous_default_session` | 无账号也能转发（环境变量旁路） | 无账号时必然 401 |
| `system_proxy_when_unset` | 未配代理时应跟随系统代理 | 该走代理的场景直连失败 |
| `refresh_uses_account` | 目录接口需要账号维度 | 「获取模型」弹窗出现一个选了也一样的下拉 |
| `supports_default_model` | 认「默认模型」这个概念 | 用户配的默认模型静默失效（或注入一个不认识的模型名） |
| `env_credentials` | 有环境变量 / 默认登录态旁路 | 广告一家「有旁路但此刻可能没凭证」的家 |

### 4.4 `AccountFeatures`

| 字段 | 含义 |
|---|---|
| `usage` | 账号卡显示「积分」按钮 |
| `identifier` | 用 secret 里哪些键做账号标识（`["uid"]` / `["userId"]`） |
| `expiry` | 用哪个键显示过期时间（`["expiresAt"]` / `["tokenExpireAt"]`） |
| `long_lived` | 凭证是否长期有效（没有续期协议但有固定长寿命时也 false） |

**没有的东西一律不申报。** 「先申报了以后再实现」的代价是用户先看到一堆点了报错的入口。

**`usage` 报告的字段契约（踩过的坑）**：网关把报告折成账号卡上的「剩余额度」时，
只读顶层 `totalLeft`（存在则优先）或 `available`，**不读 `balance`**。只给
`balance` 的表现很有迷惑性：上游请求 200、日志没有报错，但
`account_usage_records.remaining` 落成 NULL，界面就是「无法获取余额」。最小可用报告：

```json
{
  "provider": "<id>",
  "unit": "积分",
  "available": 1000,
  "balance": 1000,
  "wallets": [{ "balance": 1000, "displayName": "积分池", "type": "credits_pool" }],
  "quotaSources": [{
    "id": "account", "label": "账号主额度", "provider": "<id>",
    "remaining": 1000, "unit": "积分", "unlimited": false
  }],
  "credits": [{ "name": "赠送积分", "remain": 1000, "total": 1000, "used": 0 }]
}
```

`balance` 只是给老消费方留的别名，不能替代 `available`；`quotaSources` 首项建议带
`id: "account"`（网关从它取 `unit` 与主余额）。另外 `credits[].expireAt` /
`quotaSources[].endsAt` 必须是**毫秒数字**：上游给 ISO 8601 字符串
（如 `2026-12-12T11:02:43+08:00`）时要自己换算，否则「到期时间」永远空白。

---

## 5. 账号面：登录与凭证

### 5.1 三种登录形态

| 形态 | 用哪些方法 | 典型上游 |
|---|---|---|
| 手机验证码 | `login_start`（发码，`poll_interval_ms = 0`）+ `login_callback`（提交码） | Better Auth |
| 扫码 / 轮询 | `login_start`（返回 `url` + `poll_interval_ms`）+ `login_poll` | 各家开放平台 |
| 桌面端导入 | `login_start` 返回 `Desktop { button, hint }` | 读本机 Cookies / 配置文件 |

`LoginStart.url` 为空 + `poll_interval_ms = 0` 的语义是「用户还要填一个码，
网关不要开窗口、不要轮询，等 `login.callback`」。

### 5.2 secret 形状与账号视图

`normalize_account` 返回一条可落库的 `NormalizedAccount`：`secret` 存敏感字段，
`public` 存账号卡要显示的字段。**两者都要脱敏**：

```text
secret:  { sessionToken, accessToken, accessExpiresAt, uid, phone, email }
public:  { uid, phone: "138****8000", email, expiresAt, tokenTail: "…9f3a", displayName }
```

- 手机号 / 邮箱必须打码，令牌只留尾号；
- `account_view(secret)` 决定账号卡上的每一行；
- `account_identity(secret)` 决定**去重键**。只有令牌、没有稳定 uid 的家，用令牌
  指纹（如 FNV-1a → `name-token-<16 hex>`），否则同一账号会被重复添加。

### 5.3 令牌换发链

```rust
ensure_token   → 当前可用令牌（临期先换）
refresh_token  → 强制换一枚（401 之后用）
token_expiring → 后台维护是否该刷（判不出来就返回 false）
```

要点：

- **临期窗口**要留够（例如 60 秒），否则「刚换完就过期」。
- 401 自愈分两种位置：无状态由网关 `classify` 分类后重试；**有状态必须插件自己
  做**（网关只看到一条错误字符串），见 §6.3。
- 会话令牌过期且无法换发时，`token_expiring` 返回 `false`：救不回来就别让维护
  任务白跑。

### 5.4 账号页的两步短信技巧（无账号可返回时怎么回报成功）

`POST /api/accounts` → `account.normalize` 只有「成功返回账号」或「失败返回错误」
两条路。发码那一步没有账号可返回，于是借错误通道回报：

```text
normalize({ action: "sms-send", phone })  → Err("OTP_SENT <phone>")
normalize({ action: "sms-login", phone, code }) → Ok(account)
```

页面侧用 `error.includes("OTP_SENT")` 判定成功并开始倒计时。注意网关会给错误加
前缀（`提供商插件调用失败（account.normalize）：`），所以页面要**包含匹配**而不是
全等。

---

## 6. 数据面：无状态 vs 有状态

### 6.1 怎么选

```text
上游是 POST /chat/completions 一次一答？
├── 是 → 无状态：实现 build_chat，返回 ChatPlan
└── 否 → 有状态：申报 stateful，实现 conversation，自己驱动协议
```

**不要为了「省事」把多步协议塞进 `build_chat`。** 网关按 `stateful` 分流，
填错时请求能发出去但语义全错，且没有任何报错指向真正的原因。

### 6.2 无状态：`build_chat`

```rust
fn build_chat<'a>(&'a self, ctx: &'a PluginCtx, req: proto::ChatRequest)
    -> SendFuture<'a, ChatPlan>
{
    Box::pin(async move {
        let token = credentials::access_token(ctx, &req.account).await?;
        let mut headers = client::api_headers();
        headers.push(("Authorization".into(), format!("Bearer {token}")));
        Ok(ChatPlan::chat(format!("{}/v1/chat/completions", base), headers, req.body))
    })
}
```

`ChatPlan` 的 `body` 是 `Value` 而不是字节：网关还要改 `model`（备援改写）和注入
思考档位（`reasoning_patch`）。`upstream` 声明上游说的是哪套协议
（`Chat` / `Anthropic` / `Gemini`），网关据此选翻译机。

### 6.3 有状态：`conversation` + `ConversationSink`

插件自己发请求，往 sink 推 **OpenAI chat SSE 帧**：

```rust
async fn run(ctx, req, mut sink) -> Result<(), String> {
    // 1. 取凭证（自己处理 401：换一枚再重试一次）
    // 2. 按上游协议建会话 / 发消息 / 轮询
    // 3. 推 head，再推 chunk
    sink.head(proto::StreamHead {
        status: 200,
        headers: [("content-type".into(), "text/event-stream".into())].into(),
        upstream: proto::UpstreamKind::Chat,
    }).await?;
    for frame in frames {
        sink.send(format!("data: {frame}\n\n").as_bytes()).await?;
    }
    sink.send(b"data: [DONE]\n\n").await?;
    Ok(())
}
```

关键约定：

| 约定 | 说明 |
|---|---|
| **恒发 `stream: true`** | 网关永远以流式调用插件；非流式客户端由网关自己聚合。插件只写一条路径。 |
| **必须先 head** | 网关靠它定状态码与翻译机。先到 chunk 会被当 200 兜底，但别依赖。 |
| **head 之前的失败用 `Err`** | 网关转成 502 + 你写的文案（文案要能自解释）。 |
| **流内失败用错误帧** | 已经发过 head 后，写 `{"error":{...}}` + `[DONE]`，否则客户端只看到空白。 |
| **不要先缓冲整段** | `sink.send` 的 await 就是背压；先攒再推会让背压失效。 |

**最小可用帧序列**（上游没有 token 增量时也够用）：

```text
data: {"object":"chat.completion.chunk",...,"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}
data: {"object":"chat.completion.chunk",...,"choices":[{"index":0,"delta":{"content":"<整段回答>"},"finish_reason":null}]}
data: {"object":"chat.completion.chunk",...,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}
data: [DONE]
```

### 6.4 全局状态型的「模型选择」

有些上游没有 per-message 的模型字段，只能改**账号级设置**（Today AI 就是：
`PATCH /v1/users/me/settings {preferredModel}`）。这带来两个必须处理的后果：

1. **同一账号的请求必须串行**，否则并发请求互相改掉对方的模型；
2. 设置成功要**缓存**（账号 → 模型），否则每个请求都多一次 PATCH。

```rust
let lock = account_lock(&account).await?;   // tokio::sync::Mutex，按账号分桶
let _serialized = lock.lock().await;        // 持有到整个请求结束
```

设置失败要**如实报错**，不要「悄悄用默认模型」—— 客户端会以为自己在跟
`gpt-6-astra` 说话。

### 6.5 错误分类（`classify`）

只有四档，**不要发明第五档**（多出来的会静默落进 `Fatal`）：

| 分类 | 网关动作 | 典型判据 |
|---|---|---|
| `QuotaLimited` | 标记账号冷却 + 换账号 | 429 / 限额文案 |
| `TokenExpired` | 刷新凭证，同账号重试一次 | 401 / 403 |
| `ContentBlocked` | 不罚账号，换中性提示词重试 | 审核 / 敏感内容文案 |
| `Fatal` | 原样透传 | 其余 |

---

## 7. 模型目录

### 7.1 两个方法

```rust
fn models(&self, ctx, account) -> SendFuture<ModelList>      // 返回缓存
fn refresh_models(&self, ctx, account, force) -> SendFuture<RefreshOutcome>
```

`RefreshOutcome` 三种结果：`refreshed(n)` / `unchanged()` / `failed(msg)`。

### 7.2 三条纪律

1. **不编模型名。** 没有内置兜底就 `with_models(vec![])`，拉取失败如实报失败。
   编一个名字会让客户端选到上游根本不存在的模型。
2. **解析要容错。** 上游形状可能变：`{models:[{modelId}]}` / `{data:[{id}]}` /
   裸数组都认；`id` 与 `modelId` 都读。
3. **能力位保守。** 上游没声明就按「做不到」申报（工具位、思考位默认 false），
   否则客户端发来工具定义却永远等不到调用。

### 7.3 缓存

只在**进程内存**里缓存（自动路径 10 分钟 TTL，手动刷新 `force` 绕过）。
**不要落盘**：网关自己会缓存插件返回的清单，插件再存一份就是两套状态。

---

## 8. 构建、打包、安装

### 8.1 环境

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;C:\Users\cpyong\scoop\apps\mingw\current\bin;" + $env:Path
```

- `cargo` 可能不在 PATH（在 `%USERPROFILE%\.cargo\bin`）；
- Windows 上需要 **MinGW 先于 MSVC 工具链**；
- PowerShell 会把 cargo 的 stderr 当错误 → 命令显示 `Exited with code 1`，
  但最后一行是 `Finished` 就说明**构建成功**。

### 8.2 命令

```bash
cargo build --release -p agent2api-providers-<name>

node scripts/pack.mjs --pack crates/providers-<name> \
     --crate agent2api-providers-<name>

# 已经编过时
node scripts/pack.mjs --pack crates/providers-<name> \
     --crate agent2api-providers-<name> --skip-build
```

产物：`dist/agent2api-<name>-<version>-<target>.zip`

```text
agent2api-<name>-<version>-<target>.zip
├── pack.json
├── bin/agent2api-<name>[.exe]
└── ui/<name>/…            # 有页面时
```

`pack.mjs` 不做交叉编译；多平台由 CI 矩阵各跑一次。`--pack` 只改变清单 / 图标 /
页面的解析根目录，`cargo` 仍在工作区根目录跑（`target/` 只有一份）。

### 8.3 版本号

**每次重新打包都要 bump `Cargo.toml` 和 `pack.json` 的 `version`，两者必须一致。**
网关按 `plugins/<id>/<version>/` 解包，同版本重装会留下语义不明的旧目录。

### 8.4 安装

```text
POST http://127.0.0.1:3065/api/plugins/install
Authorization: Bearer <网关 API key>
Content-Type: application/zip
<zip 字节>
```

网关 API key 存在 `~/.agent2api/agent2api.db` 的 `kv.apiKeys`，值可能是
`enc1:` 信封：用 `~/.agent2api/account.key` 做 AES-256-GCM 解密
（`nonce(12) || ciphertext+tag`）。页面托管的 UI 不需要鉴权，管理 API 需要。

---

## 9. 插件自带页面（可选，但第三方家通常必需）

### 9.1 为什么必需

网关的「添加账号」弹窗只渲染它写死的 `BUILTIN_CONFIGS`，第三方 provider id 在那里
显示「账号添加功能即将上线」。**插件侧改不了**，所以自带页面是唯一可用的添加入口。

### 9.2 页面清单

| 文件 | 职责 |
|---|---|
| `index.html` | 结构；不要内联大段 JS |
| `api.js` | 凭证握手 + 三种响应信封 + 管理 API 调用 |
| `app.js` | 页面逻辑（表单、倒计时、列表、刷新目录） |
| `page.css` | 主题变量（`data-theme` = `light` / `dark` / `system`） |

### 9.3 凭证握手

- **web 面板**：同源，HttpOnly cookie 直接生效；
- **桌面端 / 手机**：iframe 与网关不同源，宿主 `postMessage` 递
  `{ type: "agent2api:panel-credentials", headers }`。

页面先等一小段时间（约 3 秒）的消息，拿不到就按同源继续；迟到的消息仍会被后续
请求用上。**不要**在页面里硬编码或猜 API key。

### 9.4 三种响应信封都要认

```text
{ success: true, data: … }        多数管理接口
{ success: false, error: "…" }    插件 / 账号那族
{ error: { message, type } }      OpenAI 风格（转发 / 计费）
```

只认一种的后果是错误提示变成 `[object Object]`。

### 9.5 页面常用的管理接口

| 用途 | 接口 |
|---|---|
| 列出 / 添加 / 删除账号 | `GET/POST/DELETE /api/accounts` |
| 会话（账号 + 模型） | `GET /api/session` |
| 刷新模型清单 | `POST /api/models/refresh` `{providers:["<id>"]}` |
| 模型管理（含能力位） | `GET /api/models/manage` |

---

## 10. 验证清单（端到端，逐项打勾）

### 10.1 先摸清上游（写代码之前）

用**真实令牌**把候选路径全打一遍，记录「存在 / 404 / 403」：

```python
for path in ("/v1/chat/completions", "/v1/models", "/v1/llm/models",
             "/v1/messages", "/v3/conversations", "/v1/users/me"):
    for method in ("GET", "POST"):
        ...
```

只打印非 404 的结果，能很快画出真实的路由面。

### 10.2 装好之后

| # | 检查项 | 期望 |
|---|---|---|
| 1 | `GET /api/plugins` → `packs[]` 含你的包 | `enabled: true`、`host.running: true`、`host.providers` 含你的 id |
| 2 | `GET /api/models/manage` | 目录条目出现，`source: "remote"` |
| 3 | `POST /api/models/refresh {providers:["<id>"]}` | 200，条目数正确 |
| 4 | 添加账号（页面） | 账号出现在 `/api/session`，敏感字段已打码 |
| 5 | 非流式 `POST /v1/chat/completions` | 200，`choices[0].message.content` 正确 |
| 6 | 流式同一请求 `stream: true` | 至少 `role` / 内容 / `finish_reason` / `[DONE]` 四段 |
| 7 | 多轮 + system | 回答体现 system 与历史 |
| 8 | 不存在的模型 | 网关 404 `model_not_found` |
| 9 | 删除账号 → 再请求 | 明确报「没有可用账号」，不是 500 |
| 10 | 令牌过期（等或手工造） | 自动续期后成功，或给出可读的「请重新登录」 |

第 5–8 项都可以用同一个脚本对 `http://127.0.0.1:3065/v1/chat/completions` 打，
鉴权用网关 API key（不是上游令牌）。

---

## 11. 踩坑速查

| 现象 | 原因 | 处置 |
|---|---|---|
| `pack.mjs` 报 JSON 解析失败 | `pack.json` 带 BOM | 用无 BOM 方式写文件 |
| `cargo` 找不到 | 不在 PATH | 手动加 `%USERPROFILE%\.cargo\bin` |
| 构建命令 exit 1 但已 `Finished` | PowerShell 把 stderr 当错误 | 看最后一行，忽略退出码 |
| `cargo clippy` 报 toolchain 缺 manifest | 该工具链没装 clippy | 跳过，靠 `#![deny]` 与 review |
| 装了包但 provider 不出现 | 版本号没 bump / 同 id 重复安装 | bump 版本；两个包只装一个 |
| 「添加账号」显示即将上线 | 内置弹窗的 `BUILTIN_CONFIGS` 不含第三方 | 用插件自带页面 |
| 管理 API 401 | 没带网关 API key | 从 `kv.apiKeys` 解密后带上 |
| 页面错误提示 `[object Object]` | 只认了一种响应信封 | 三种都认 |
| 有状态转发 502「在给出响应头之前退出」 | 插件 panic / 上游请求失败前没给 head | 去掉 `unwrap`；失败用 `Err` 带文案 |
| 长轮询总超时 | 复用了 20 秒的控制面客户端超时 | 逐请求覆盖 timeout |
| 并发请求拿到别人的模型 | 模型是账号级设置 | 按账号串行 |
| 客户端收不到工具调用 | 上游给不出 `tool_calls` | 目录申报 `supportsToolCall: false` |
| 模型名对不上 | 上游回内部名 | `sse_model_rewrite` 或改写请求体 |

---

## 12. 提交前检查

- [ ] `cargo build --release -p <crate>` 通过，零 `unwrap/expect/panic`
- [ ] `Cargo.toml` 与 `pack.json` 的版本一致且已 bump
- [ ] `pack.json` 无 BOM，`entry` 覆盖五个目标三元组
- [ ] 没有申报做不到的 `capabilities`（`checkin` / `usage` / `claims` 尤其）
- [ ] 上游失败路径都有可读文案（谁该做什么，而不是「请求失败」）
- [ ] 账号视图里手机号 / 邮箱 / 令牌已打码
- [ ] README（中英）写清：能力边界、上游形态、已知限制
- [ ] 端到端清单 §10.2 全绿
- [ ] 产物 zip 已实际安装并跑过一次真实请求

---

## 13. 案例：Today AI 的关键结论

留作「上游不是标准 OpenAI 兼容」时的参考。

| 事实 | 结论 |
|---|---|
| `/v1/chat/completions`、`/v1/responses`、`/v1/llm/*`（除目录）全 404 | 不存在 OpenAI 兼容推理接口 |
| `/v1/llm/models` → `{models:[{modelId}]}`，35 个 | 目录路径与形状都不是 OpenAI 默认 |
| `/v1/messages` 是**聊天线程**接口，不是 Anthropic Messages | 发 Anthropic 体会 `400 Invalid request` |
| `/v3/conversations` + `/v3/messages` + `message-changes` | 唯一能跑模型的通道 → `stateful` |
| `PATCH /v1/users/me/settings {preferredModel}` 校验模型名 | 模型是账号级设置，不是 per-message |
| `message-changes` 只给整条消息 | 流式只有一帧正文，没有打字机效果 |
| 回答由用户的个人助理产生（带记忆与连接器） | 插件无法关闭，只能「每次开新会话、不碰主会话」 |

对应的实现选择：`stateful: true`、每次请求新建会话、按账号串行、
`supportsToolCall/ supportsReasoning = false`、目录来自 `/v1/llm/models`。

---

## 14. Java 复刻版参考实现（`agent2Api-java`）

Today AI 的协议逻辑已用**纯 JDK（零依赖）**复刻了一遍，落在
`E:\workspace\git\agent2Api-java`。它不依赖网关、不依赖插件进程、也不依赖任何
第三方包——把「上游协议是什么」与「插件载体长什么样」分开了：想先独立钉死
一个上游的协议（目录怎么拉、模型怎么调），再用 §2 的骨架把它塞回插件 crate，
这是比「边写插件边猜协议」更稳的开发顺序（对应 §10.1 的进阶版）。

### 14.1 与 Rust 插件版的模块对应

| Java（`src/todayai/`） | 对应 Rust | 说明 |
|---|---|---|
| `Json.java` | serde_json | 200 行解析/序列化；`Json.str/num/arr/obj` ≈ `get().and_then(as_xxx)` |
| `HttpJson.java` | `http.rs` | `java.net.http.HttpClient`；**超时按请求覆盖**（控制面 30s / 数据面 180s） |
| `Endpoints.java` | `endpoints.rs` | 三基址 + 客户端标识头，环境变量可覆盖 |
| `Errors.java` | `client.rs::describe_error` | 错误码 → 人话（500 ≠「手机号不对」的教训在这） |
| `Credentials.java` | `credentials.rs` + `client.rs` | 两层令牌；临期 60s 主动换发；401 自愈换一枚重试一次 |
| `Models.java` | `models.rs` | `GET /v1/llm/models`；三形状容错解析；10 分钟 TTL 进程内缓存 |
| `ChatBridge.java` | `chat.rs` | 有状态桥：账号级 `synchronized` 串行、`preferredModel` 缓存、v3 三步、长轮询（挂起 25s / 稳定确认 1.2s / 总预算 150s）、OpenAI 三帧 SSE |
| `Server.java` | 网关（agent2api.exe） | 用 `com.sun.net.httpserver` 模拟 `/v1/models` 与 `/v1/chat/completions`（流式 + 非流式聚合） |
| `Main.java` | — | CLI：`list-models` / `chat` / `serve` |

### 14.2 构建与运行

```powershell
powershell -File scripts\build.ps1        # javac 全量编译到 out\（--encoding UTF-8）
$env:TODAYAI_SESSION_TOKEN = "<手机短信登录的会话令牌>"
java -cp out todayai.Main list-models [--force]
java -cp out todayai.Main chat --model glm-5.3-flash "你好"
java -cp out todayai.Main serve 8080      # OpenAI 兼容端点；JAVA_API_KEY 设置后校验 Bearer
```

### 14.3 复刻时的可移植经验（跨语言通用的坑）

| Rust 版机制 | Java 版代价 / 处置 |
|---|---|
| `json!` 宏 + `Value` 随手 gang 一个 key | 必须有一个显式 JSON 库或自写解析器；`{models:[…]}`、`{data:[…]}`、裸数组三种形状照旧全认 |
| `tokio::sync::Mutex` 按账号分桶 | `ConcurrentHashMap.computeIfAbsent(account, new Object)` + `synchronized`，语义一致（账号级全局设置 → 必须串行） |
| `sink.head + sink.send`（SDK 背压） | `sendResponseHeaders(200, 0)` + 每帧 `flush()`；帧内容逐字节对齐 Rust 版 `emit` |
| 网关做 SSE 聚合 | 没有网关就自己用内置 HttpServer 聚合——非流式 = 把整段回答装回 `chat.completion` 形态 |
| `reqwest` 逐请求 timeout | `HttpClient` 全局 `connectTimeout` + `request.timeout()` 逐请求；长轮询 25s 复用 20s 级超时必炸（同 §11） |
| `Idempotency-Key` 随手生成的短 id | 不引 uuid 库：时间戳 + 自增序号足够（同 `unique_id()`） |

### 14.4 复刻版的实测状态（截至本文写作时）

| 验证项 | 结果 |
|---|---|
| `list-models`（真实会话令牌） | ✅ 35 个模型，与插件版目录一致 |
| `chat --model glm-5.3-flash` | ✅ 设置模型 → v3 三步 → 长轮询 → 返回整段回答 |
| `serve`（OpenAI 兼容端点） | ⚠️ `/v1/models` 的 handler 挂住问题排查中，尚未定案 |

> 复刻版的价值在协议，不在产品形态：`Json/Endpoints/Credentials/Models/ChatBridge`
> 五个类几乎是插件 crate 的逐行翻译，可作为「上游协议文档缺失时的活文档」。

---

## 15. 反模式清单

1. **先编模型名再想办法**：客户端会选到不存在的模型，错误发生在最远的地方。
2. **把多步协议塞进 `build_chat`**：`stateful` 分流被绕过，语义全错且无报错。
3. **申报没有的能力**：每个假入口都会变成一张 bug 单。
4. **在流式路径里先缓冲整段**：背压失效，大回答会把插件内存打满。
5. **有状态通道里指望网关做 401 分类**：网关只看到错误字符串，必须插件自己换令牌重试。
6. **把令牌 / 手机号原样放进 `public`**：账号卡会直接显示出来。
7. **插件自己落盘模型清单**：与网关的缓存形成两套状态，刷新后不一致。
8. **同一个 provider id 装两份实现**：网关按 id 认实现，行为不确定。
