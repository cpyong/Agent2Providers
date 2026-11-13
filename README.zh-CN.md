# Agent2Providers

[English](README.md) · **简体中文**

**提供商增强**是 [Agent2API](https://github.com/aimod-cc/Agent2API) 的官方提供商插件，扩展账号登录与导入、模型调用、
凭证续期、模型目录、余额查询和签到等能力，具体功能以各提供商的支持情况为准。
网关把插件作为独立进程加载，因此某个提供商出故障只会让那个进程挂掉，
不会影响网关其他部分。

本仓库同时承载网关编译时所依赖的两个 crate：`agent2api-provider-protocol`
（线上类型）和 `agent2api-provider-sdk`（`Provider` trait 及其 serve 循环）。
网关通过 tag 引用本仓库。

## 支持的提供商

「提供商增强」插件的支持情况一览。下表只列当前启用的 Rust 适配；静态目录、用量和订阅方案信息不等同于在线目录或货币余额。

| 平台 | LLM 请求 | Token 自动续期 | 模型列表 | 余额查询 | 签到 | 领取类 |
| --- | :--: | :--: | :--: | :--: | :--: | :--: |
| WorkBuddy 国内版 | ✓（对话 / 生图 / 参考图编辑） | ✓ | ✓ | ✓ | ✓ | ✓ |
| WorkBuddy 国际版 | ✓ | ✓ | ✓ | ✓ | ✗ | — |
| 小浣熊 | ✓（对话 / 生图 / 参考图编辑） | ✓（旧凭证；IAM 需重登录） | ✓ | ✓ | ✓ | ✓（首次登录奖励） |
| CatPaw | ✓ | ✗ | ✓ | ✓ | ✗ | — |
| AutoClaw（国内版 / 国际版） | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| Qoder | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| 千问办公（国内版 / 国际版） | ✓ | ✓ | ✓ | ✓ | ✗ | — |
| Cline（Free / Pass） | ✓ | ✓ | ✓ | ✓ | — | — |
| Accio（国际版 / 国内版） | ✓ | ✓ | ✓ | ✓ | — | — |
| ZCode（国内版 / 国际版） | ✓ | ✗ | ✗ | ✓ | — | ✓ |
| CodeArts | ✓ | ✓ | ✓ | ✓ | — | ✓ |
| Trae | ✓ | ✓ | ✓ | ✓ | ✓ | 本插件不提供入口 |
| Loomy（讯飞） | ✓（对话 / 生图 / 参考图编辑） | ✗ | ✓ | ✓ | ✓ | — |
| KukuAI（百度文库） | ✓ | ✗ | ✓ | ✓ | ✓ | — |
| MonkeyCode（长亭科技，国内版 / 国际版） | ✓ | ✗ | ✓ | — | — | — |
| Command Code | ✓ | ✗ | ✓ | — | — | — |
| Antigravity（Google，Gemini / Claude / GPT-OSS） | ✓（对话 / 生图 / 参考图编辑） | ✓ | ✓ | — | — | — |
| OfficeAce（华为云果办 / OfficeClaw） | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| OrcaTerm（腾讯云遨驰终端） | ✓ | ✓ | ✓ | ✓ | — | — |
| ModelScope（魔搭，国内版 / 国际版） | ✓ | ✗ | ✓ | ✓ | ✓ | — |
| AtomCode（AtomGit / GitCode） | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| LobsterAI（网易有道「有道龙虾」） | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| 豆包工作 | 已实现，待实测（对话 / 生图） | ✗ | 已实现，待实测 | 已实现，待实测 | 已实现，待实测 | 已实现，待实测 |
| Kiro | ✓（对话 / 工具 / 思考 / 图片输入） | ✓ | ✓ | ✓ | ✗ | — |
| AStudio（科大讯飞 Astron Studio） | ✓（对话 / 工具 / 思考 / 图片输入） | ✗ | ✓ | ✓ | ✓（每日登录积分） | — |

自定义提供商仍由网关本体提供，无需安装本插件；支持模型请求与模型列表获取，其他能力取决于你配置的服务。API 添加账号只在 `proxySaved: true` 时跳过第二次代理保存；旧网关走兼容保存并保留核查告警，失败不撤销已添加账号，也不假称代理已保存。

> 注：模型列表标记表示在线拉取或静态目录，以该行及实际实现为准。不能仅凭缺少续期或 usage hook 判定上游不存在接口。Qoder 签到仅中国版可用；千问办公两版都**没有**签到接口（也没有个人访问令牌与领取类活动，上游确实没有这三样，不是「还没做」）。
>
> 千问办公的登录方式：**国内版**走官方 OAuth 授权页（授权码 + 本机回调，官方 CLI / IDE 同一条路），登录后账号自动加入；
> **国际版已从列表隐藏**（2026-11-03）：它的对话与余额链路是通的，但登录被上游挡住 —— 上游只接受它自己注册的回调地址，
> 授权码回不到本机，设备码入口也已停用，等于加不进账号。插件代码仍然保留，网关的停用名单（`availability.rs`）移出它即可恢复。
>
> Kiro：浏览器登录走 Kiro 门户（Google / GitHub / AWS Builder ID / IAM Identity Center），回调落在本机 loopback 端口，**网关与本机浏览器需同机**；远程部署请用「导入 kiro-cli / Kiro IDE 登录态」或 API key。上游没有签到与领取类活动。
> AStudio：模型调用走 OpenAI Responses；余额与每日登录积分需要账号带网页会话。登录支持两种：手机号验证码 / 账号密码登录（在应用内嵌的本机登录页完成，验证码发送与密码登录需过极验滑块；网关与浏览器需同机）、粘贴 modelBearerToken（需另填 token 与 accountId 才有余额与签到）。凭据失效只能重新登录或粘贴，没有续期接口。

## 构建

```bash
cargo build --release            # 当前平台
node scripts/pack.mjs            # 生成可安装的 zip（会先执行上面的构建）
```

zip 输出到 `dist/agent2api-providers-<version>-<target>.zip`。

要构建到其他目标平台，需自行准备工具链并指定目标：

```bash
cargo build --release --target aarch64-unknown-linux-gnu
node scripts/pack.mjs --skip-build --target aarch64-unknown-linux-gnu
```

该脚本只负责把一个已经构建好的二进制打成 zip，不做交叉编译 —— 那需要
mingw / osxcross / musl 工具链，把它塞进来只会多出一个注定失败的分支。
多平台构建是 CI 矩阵的职责（`.github/workflows/build.yml`，每个平台一个 job）。

### 独立包：Today AI

Today AI **不在合集包里**，而是 `crates/providers-todayai` 下的独立插件包
（包 id `agent2api-todayai`，只含 `todayai` 一家）。构建与打包都只针对它：

```bash
cargo build --release -p agent2api-providers-todayai
node scripts/pack.mjs --pack crates/providers-todayai \
     --crate agent2api-providers-todayai
# → dist/agent2api-todayai-<version>-<target>.zip
```

`--pack` 只改变清单与图标 / 页面的解析根目录；`cargo` 仍在工作区根目录跑
（`target/` 只有一份）。两个包**不要同时安装** —— provider id 都是 `todayai`，
网关会把它们当成同一家的两份实现。

#### 怎么添加账号

在侧边栏打开 **Today AI 账号** 页添加（本包自带页面，源码在 `ui/todayai/`）。
网关内置的「添加账号」弹窗只渲染它自己写死的 `BUILTIN_CONFIGS` 那十几家，
第三方插件家的 id 在那里只会显示「账号添加功能即将上线」—— 这一点插件侧
改不了。

该页与内置页用同一套管理 API：手机号 + 短信验证码（两步都经
`POST /api/accounts` 进到插件的 `account.normalize`，用 `action: "sms-send"` /
`"sms-login"` 区分）、粘贴已有的 `sessionToken` / JWT、查看与删除账号、刷新
模型清单。添加完成后账号立刻参与路由。

#### 请求是怎么转出去的

Today AI **没有** OpenAI 兼容的推理接口：`/v1/chat/completions`、
`/v1/responses` 以及除 `/v1/llm/models` 之外的 `/v1/llm/*` 全是 404。真正跑
模型的只有助理会话接口，所以 `todayai` 申报 `stateful`，由插件自己驱动整条
会话：

1. `PATCH /v1/users/me/settings` → `preferredModel`（模型是**账号级**设置，
   因此同一账号的请求串行）；
2. `POST /v3/conversations` → 每次请求开一个独立会话；
3. `POST /v3/messages` → 发送 prompt（`messages` 被压成一段文本）；
4. `GET /v3/conversations/{id}/message-changes` → 长轮询取回答；
5. 翻译成 OpenAI `chat.completion.chunk` 帧并补 `data: [DONE]`。

下面这些是上游形态带来的，不是实现选择：

- 回答来自**你的个人 Today 助理**，它带着你的记忆与已连接的连接器。插件
  关不掉这些，能做的只是「不复用主会话、不往主会话里写」；
- 上游没有 token 增量 —— 流式响应把整段回答放在一帧里（`finish_reason` 与
  `[DONE]` 照常给）；
- 没有工具调用、没有思考流，因此目录里申报 `supportsToolCall: false` /
  `supportsReasoning: false`；
- 同一账号同时只跑一个请求（模型设置是全局的）。

模型清单来自 `GET /v1/llm/models`（35 个 id）—— OpenAI 形状的 `/v1/models`
在那里是 404。

#### 上游的「软拒绝」与提示词清洗

上游对它不想接的输入**不报错**，而是正常落一条助理消息，正文是一句固定
话术：

```
我暂时无法处理这条消息，请稍后重试或换个话题。
```

这是拒答，不是回答。实测触发面很宽：把 Cursor 的 `<user_info>`（十几 KB，
内含 `<git_status>`、技能清单、MCP 工具目录）和 `<rules>`（用户自定义规则，
常是上万字的「必须 / 禁止」祈使句）原样塞进 `body.text` 时几乎必现；普通
对话历史堆到四五万字符也开始出现。

插件对此做三件事（`crates/providers-todayai/src/todayai/chat.rs`）：

1. **清洗**：`<git_status>` / `<agent_skills>` / `<mcp_meta_tools>` /
   `<open_and_recently_viewed_files>` 等客户端元数据段整段丢弃；`<rules>` /
   `<user_rule>` 只留开头 1.2 K 字；`<user_info>` 只留前 600 字（OS、Shell、
   工作区路径都在最前面）；
2. **预算**：整段提示词压到 3 万字符以内，当前问题优先（1.2 万），历史从近
   到远装，单条最多 4 千；被丢掉的历史会在开头标一句「更早的 N 条已省略」；
3. **重试与报错**：认出这句拒答就自动换一份「只带最近两轮」的精简提示词，
   在**新会话**里再试一次；仍然拒答则报错并说明原因，绝不把话术伪装成回答。

一句话：宁可让客户端看到一次可重试的错误，也不要让它以为模型只回了这一句。

### 独立包：OpenCode

`crates/providers-opencode` 是另一个独立插件包（`agent2api-opencode`），内含
**两家** provider —— `stateful` 是整家静态能力位，两条上游协议不同，不能按
账号切换：

| id | 上游 | 状态 |
|---|---|---|
| `opencode-local` | 本机 opencode 桌面端 / `opencode serve` 的 v2 API（`/api/*`） | **推荐**，实测可用 |
| `opencode` | `opencode.ai/inference` 匿名车道 | 有出口 IP 免费额度，可能 429 |

构建与打包：

```bash
cargo build --release -p agent2api-providers-opencode
node scripts/pack.mjs --pack crates/providers-opencode \
     --crate agent2api-providers-opencode
# → dist/agent2api-opencode-<version>-<target>.zip
```

**为什么要有本地这一家。** 云端匿名车道 `GET /v1/models` 返回 84 个模型，
但 `POST /v1/chat/completions` 对免费模型回 `429 FreeUsageLimitError`
（"Free models work in OpenCode"，`Retry-After` 约 14 小时）；额度按出口 IP
计，换账号 / 换 Key 都无效。而同样的免费模型**通过本机 opencode 客户端自己的
v2 API 能正常回答**。那个 API 不是 OpenAI 兼容端点：截图里「设置 → 服务器」
的连接是前端页面 + v2 REST API，未匹配路径一律回落 SPA，所以把
`http://127.0.0.1:<端口>` 填进「自定义上游基址」拼 `/v1/chat/completions`
只会拿到一屏 HTML。

`opencode-local` 申报 `stateful`，由插件自己驱动会话：

1. `GET /api/model` 取模型清单；
2. 先订阅 `GET /api/event` 全局 SSE（晚连会丢增量）；
3. `POST /api/session` 建会话，带 `{model:{id,providerID}}`；
4. `POST /api/session/{id}/prompt` 发 `{text}`（多轮 messages 拍平成一段带
   角色标签的文本，每次请求新建会话）；
5. `session.text.delta` 事件 → OpenAI `chat.completion.chunk`，最后补
   `finish_reason:"stop"` 与 `data: [DONE]`；SSE 断开 / 超时则回退到轮询
   `/api/session/{id}/message` 取全文，只补推未发出的部分；
6. `DELETE /api/session/{id}` 清理会话。

**怎么用。** 先启动 opencode 桌面端（或 `opencode serve`），打开本包的
**OpenCode 账号** 页点「自动发现并连接」。插件读
`~/.local/state/opencode/service.json` 拿地址与密码（Basic auth，用户名默认
`opencode`）。端口每次启动随机分配，因此自动发现的凭证**故意不落盘地址与
密码**，每次请求实时重读；也可以手填。注意：转发期间桌面端必须一直运行。

**限制。** v2 prompt 只接受一段文本，工具调用 / 结构化输出 / `logprobs`
无法映射，只保证纯文本对话；SSE 是全局流，按 `sessionID` 过滤。

云端那家的模型清单已改成取自**实际转发用的**
`inference/openai/v1/models`，按 `cost.text.*.price == 0` 筛出 12 个免费模型
（旧版用全站能力目录，广告 51 个模型里有 39 个这条车道根本不提供）。
`429 FreeUsageLimitError` 归为 `Fatal` 而不再标成限额 —— IP 额度换账号救不回来。

### 独立包：Meoo 秒悟

`crates/providers-meoo` 是独立安装的包（`agent2api-meoo`），只含 `meoo` 一家：

```bash
cargo build --release -p agent2api-providers-meoo
node scripts/pack.mjs --pack crates/providers-meoo \
     --crate agent2api-providers-meoo
# → dist/agent2api-meoo-<version>-<target>.zip
```

从包自带的 **Meoo 秒悟** 页面（`ui/meoo/`）添加账号。和其它独立包一样，网关
内置「添加账号」弹窗只渲染硬编码的家清单，第三方 provider id 在那里会显示
「即将上线」。

**两套凭证、两个平面，不能互换：**

| 平面 | 地址 | Bearer | 用途 |
| --- | --- | --- | --- |
| 控制面 | `https://meoo.com/open/v1/cli-compat/…` | OAuth access token | 身份、积分、权益、项目、服务 AK |
| AI 网关 | `https://api.meoo.host/meoo-ai/compatible-mode/v1` | 项目服务 AK（`meoo_project_service_v1.*`） | `/models`、`/chat/completions` |

只有服务 AK 的账号**能聊天、能拉模型目录，但查不了积分也签不了到** —— 控制面
不认 AK 当 Bearer。浏览器登录会一次拿齐两套。

**登录。** 按钮在系统浏览器打开官方 OAuth 授权页（授权码 + PKCE，
`scope=cli.compat`，动态 loopback `redirect_uri`）。插件自己 `bind("127.0.0.1:0")`
收回调、换 token、读身份、尽量当场取到项目服务 AK；不读 Cookie，也不碰用户
日常浏览器的登录态。页面另提供「手填服务 AK」兜底。

> 插件自有页面运行在网关的 webview / iframe 里，`window.open` 与
> `<a target="_blank">` 常被引擎策略拦掉（流程在跑、却始终没有窗口），因此
> **由插件进程拉起系统默认浏览器**：Windows 走 `rundll32
> url.dll,FileProtocolHandler`，不经过 cmd，避免百分号编码的 `redirect_uri`
> 被环境变量展开。页面同时明文显示完整授权地址，提供「重新用系统浏览器打开」
> 与一键复制作为最后兜底。无桌面 / 自动化环境可设 `MEOO_OPEN_BROWSER=0`
> 关闭自动开窗。

**签到在上游没有独立接口。** 官方文档（`docs.meoo.com/coindesc`）写的是
「所有用户每日首次登录系统，均可免费获赠 2000 积分」，没有 claim / 任务端点。
因此 `checkin.claim` 做一次令牌续期（等价于登录）+ 一次 `/account/identity`
会话 touch，再最多回读两轮 `/account/points`，**只有余额真的涨了**才报
`Claimed`；余额没涨但权益有效报 `Done`（今日已领），连余额都读不到报
`Failed`。`claim.status` 严格只读，绝不触发登录。这与 WPS 灵犀包同款取证纪律，
只是少了「任务状态回查」—— 上游没有那个端点。

`build_chat` 是无状态的：插件只把网关 URL / 头 / 体交给网关自己发，不中转 SSE 流。

## 每个平台都需要自己的包

插件是网关直接启动的原生可执行文件，因此每个操作系统与架构的组合都需要
各自的二进制。`pack.json` 声明了五种：

| 目标三元组 | 覆盖范围 |
|---|---|
| `x86_64-pc-windows-msvc` | Windows 桌面（x64） |
| `x86_64-apple-darwin` | Intel 芯片的 macOS |
| `aarch64-apple-darwin` | Apple Silicon 的 macOS |
| `x86_64-unknown-linux-gnu` | Linux 桌面 amd64、Docker amd64 |
| `aarch64-unknown-linux-gnu` | Docker arm64 |

网关会检查当前平台的入口点是否存在于包中，缺失则拒绝安装。更新检查按资源名
`<id>-<version>-<target>.zip` 精确匹配，所以文件名里的平台段不能省略。

**两个 Linux 包都基于 bookworm 这一代构建。** Docker 运行时镜像是
`debian:bookworm-slim`（glibc 2.36）。在更新的发行版（如 Ubuntu 24.04，
glibc 2.39）上构建出的二进制会要求更新的 glibc，在容器内会以动态链接器报错
的方式失败。CI 在 Ubuntu 22.04 runner（glibc 2.35）上构建两个 Linux 目标，
低于运行时镜像的要求，因此能在其上运行。

## 签名（可选）

```bash
node scripts/sign.mjs keygen                                    # 执行一次
node scripts/sign.mjs sign --zip dist/agent2api-providers-*.zip
```

私钥默认写到 `~/.agent2api/plugin-signing-key.json`（或 `--key` /
`AGENT2API_PLUGIN_KEY` 指定的位置），**绝不**进入仓库。签名是对包内容做
ed25519 签名 —— 包括 id、version、protocol、target、providers、入口点以及
入口二进制的 sha256 —— 而不是对 zip 字节签名。生成密钥后，要把打印出的
公钥放进网关的 `OFFICIAL_PUBKEYS`（`plugin_host/install.rs`）；否则网关
不会信任它，会以签名无效为由拒绝安装。

未签名的包也能正常安装和使用。界面会以琥珀色标注「unsigned」（未签名），
这只表示无法证明来源。

## 验证

```bash
cargo build --release
node scripts/verify.mjs          # 默认使用 target/release 下的二进制
node scripts/verify.mjs --exe path/to/agent2api-providers
```

该脚本扮演网关角色，对插件二进制运行协议：init、请求、流式、背压、取消、
回调以及崩溃恢复。全部 28 项检查必须通过。

CI 的每个平台 job 都会运行它 —— **能编译不等于能运行**。缺失的动态库、
缺失的可执行权限位或错误路径，只有在启动时才会暴露，因此每个平台的二进制
都会先在自己的机器上启动一次，之后才产出包。

## 两个共享 crate

- `agent2api-provider-protocol` —— 线上类型（协议契约）；网关与每个插件都会编译它
- `agent2api-provider-sdk` —— `Provider` trait 与 `serve()` 循环

两者都在本仓库的 `crates/` 下，`crates/providers/Cargo.toml` 通过 workspace
内的相对路径引用它们。协议改动在本地即可生效，无需发布。

网关则以 **tag** 方式引用同一个仓库：

```toml
agent2api-provider-protocol = { git = "https://github.com/ProvidersPlugin/Agent2Providers", tag = "v0.2.1" }
```

因此协议改动只有在打新 tag 并更新网关那行引用之后，才会到达网关。只改本地
副本会让网关继续按旧契约编译；字段不匹配仍可能编译通过，但在运行时失败。

## License

AGPL-3.0
