# Agent2Providers

**English** · [简体中文](README.zh-CN.md)

**Provider Enhancement** (提供商增强) is the official provider plugin for [Agent2API](https://github.com/aimod-cc/Agent2API).
It supplies provider-specific sign-in and account import, model requests, credential
renewal, model catalogs, balances and check-in. Available capabilities vary by provider.
The gateway loads the plugin as a separate process, so a fault in one provider
takes down that process and nothing else.

This repository also carries the two crates the gateway compiles against:
`agent2api-provider-protocol` (the wire types) and `agent2api-provider-sdk`
(the `Provider` trait and its serve loop). The gateway depends on this
repository by tag.

## Supported Providers

Capabilities supplied by Provider Enhancement. The table lists the currently enabled Rust integrations. Static model catalogs, usage and plan information are not equivalent to online catalogs or monetary balances.

| Platform | LLM requests | Token auto-refresh | Model list | Balance query | Check-in | Claims |
| --- | :--: | :--: | :--: | :--: | :--: | :--: |
| WorkBuddy (China) | ✓ (chat / image generation / reference editing) | ✓ | ✓ | ✓ | ✓ | ✓ |
| WorkBuddy (Global) | ✓ | ✓ | ✓ | ✓ | ✗ | — |
| Raccoon | ✓ (chat / generation / reference editing) | ✓ (legacy credentials; IAM requires sign-in) | ✓ | ✓ | ✓ | ✓ (first-login rewards) |
| CatPaw | ✓ | ✗ | ✓ | ✓ | ✗ | — |
| AutoClaw (domestic / international) | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| Qoder | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| QwenWork (China / International) | ✓ | ✓ | ✓ | ✓ | ✗ | — |
| Cline (Free / Pass) | ✓ | ✓ | ✓ | ✓ | — | — |
| Accio (international / domestic) | ✓ | ✓ | ✓ | ✓ | — | — |
| ZCode (domestic / international) | ✓ | ✗ | ✗ | ✓ | — | ✓ |
| CodeArts | ✓ | ✓ | ✓ | ✓ | — | ✓ |
| Trae | ✓ | ✓ | ✓ | ✓ | ✓ | Not provided by this plugin |
| Loomy (iFlytek) | ✓ (chat / image generation / reference editing) | ✗ | ✓ | ✓ | ✓ | — |
| KukuAI (Baidu Wenku) | ✓ | ✗ | ✓ | ✓ | ✓ | — |
| MonkeyCode (Chaitin, domestic / international) | ✓ | ✗ | ✓ | — | — | — |
| Command Code | ✓ | ✗ | ✓ | — | — | — |
| Antigravity (Google, Gemini / Claude / GPT-OSS) | ✓ (chat / image generation / reference editing) | ✓ | ✓ | — | — | — |
| OfficeAce (Huawei Cloud, OfficeClaw) | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| OrcaTerm (Tencent Cloud) | ✓ | ✓ | ✓ | ✓ | — | — |
| ModelScope (MoDa, domestic / international) | ✓ | ✗ | ✓ | ✓ | ✓ | — |
| AtomCode (AtomGit / GitCode) | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| LobsterAI (NetEase Youdao, Youdao Lobster) | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| Doubao Work | Implemented; unverified (chat / image generation) | ✗ | Implemented; unverified | Implemented; unverified | Implemented; unverified | Implemented; unverified |
| Kiro | ✓ (chat / tools / thinking / image input) | ✓ | ✓ | ✓ | ✗ | — |
| AStudio (iFlytek Astron Studio) | ✓ (chat / tools / thinking / image input) | ✗ | ✓ | ✓ | ✓ (daily sign-in credits) | — |

Custom providers remain a gateway feature and do not require this plugin. They support model requests and model-list fetching; other capabilities depend on the service you configure. API account creation skips a second proxy save only for `proxySaved: true`; older gateways retain a compatibility save and verification warning. Failure does not remove an added account or falsely report that its proxy was saved.

> Note: model-list markers cover online fetching or static catalogs as specified by each row and implementation. Missing refresh or usage hooks do not establish that the upstream lacks an endpoint. Qoder check-in is China-version only; QwenWork has **no** check-in endpoint on either edition (nor a personal access token or claimable campaigns — those three genuinely do not exist upstream, rather than being "not implemented yet").
>
> QwenWork sign-in: the **China edition** uses the official OAuth page (authorization code with a loopback callback, the same route the official CLI / IDE takes) and the account joins the list by itself. The **international edition is hidden from the list** (2026-11-03). Its chat and usage paths work, but sign-in is blocked upstream: the upstream accepts only its own registered callback address, so the code never reaches this machine, and its device-code endpoint has been retired — an account simply cannot be added. The code stays in place; removing the id from the gateway's suspended list (`availability.rs`) re-enables it.
>
> Kiro: browser sign-in goes through the Kiro portal (Google / GitHub / AWS Builder ID / IAM Identity Center) and the callback lands on a loopback port, so the gateway and the browser must be on the same machine. For remote deployments use "import the kiro-cli / Kiro IDE sign-in" or an API key. The upstream has no daily check-in and no claimable campaigns.
> AStudio: model calls use OpenAI Responses; balance and daily sign-in credits require the account to carry a web session. Two sign-in routes: phone code or account/password on the in-app local sign-in page (sending a code and password sign-in require the GeeTest slider; the gateway and the browser must share one machine), or paste a `modelBearerToken` (fill `token` and `accountId` too for balance and check-in). Credentials can only be re-logged-in or re-pasted — the upstream has no refresh endpoint.

## Building

```bash
cargo build --release            # current platform
node scripts/pack.mjs            # produce an installable zip (runs the build above)
```

The zip lands in `dist/agent2api-providers-<version>-<target>.zip`.

To build for another target, prepare the toolchain yourself and name it:

```bash
cargo build --release --target aarch64-unknown-linux-gnu
node scripts/pack.mjs --skip-build --target aarch64-unknown-linux-gnu
```

The script only turns an already-built binary into a zip. It does no cross
compilation — that needs mingw / osxcross / musl toolchains, and folding them
in would add a branch that can only fail. Multiple platforms are the CI
matrix's job (`.github/workflows/build.yml`, one job per platform).

### Standalone pack: Today AI

Today AI is **not** part of the bundle. It lives in `crates/providers-todayai`
as its own installable pack (pack id `agent2api-todayai`, containing only
`todayai`). Build and pack it on its own:

```bash
cargo build --release -p agent2api-providers-todayai
node scripts/pack.mjs --pack crates/providers-todayai \
     --crate agent2api-providers-todayai
# → dist/agent2api-todayai-<version>-<target>.zip
```

`--pack` only changes where the manifest, icons and UI pages are resolved;
`cargo` still runs at the workspace root (there is a single `target/`). Do not
install both packs at once — they both declare provider id `todayai`.

#### Adding an account

Add accounts from the **Today AI 账号** page in the sidebar (the pack ships its
own page in `ui/todayai/`). The gateway's built-in “Add account” dialog renders
only the providers in its hardcoded `BUILTIN_CONFIGS` table, so a third-party
provider id shows “coming soon” there — no plugin can change that.

The page uses the same management API as the built-in pages: phone number + SMS
code (the two steps are routed through `POST /api/accounts` → the plugin's
`account.normalize`, distinguished by `action: "sms-send"` / `"sms-login"`),
paste an existing `sessionToken`/JWT, list and delete accounts, and refresh the
model catalog. Added accounts take part in routing immediately.

#### How requests are forwarded

Today AI has **no** OpenAI-compatible inference endpoint: `/v1/chat/completions`,
`/v1/responses` and every `/v1/llm/*` variant except `/v1/llm/models` return
404. The only thing that runs a model is the assistant thread API, so `todayai`
is declared `stateful` and the plugin drives the whole conversation:

1. `PATCH /v1/users/me/settings` → `preferredModel` (the model is an
   **account-level** setting, so requests are serialised per account);
2. `POST /v3/conversations` → a fresh private conversation per request;
3. `POST /v3/messages` → the prompt (`messages` are flattened into one text);
4. `GET /v3/conversations/{id}/message-changes` → long-poll for the reply;
5. emit OpenAI `chat.completion.chunk` frames plus `data: [DONE]`.

Things that follow from the upstream shape, not from this implementation:

- replies come from **your personal Today assistant**, which keeps your memory
  and connected connectors. The plugin cannot disable those; it only avoids
  writing into your Main Chat and does not reuse its history;
- upstream has no token deltas — a streaming reply carries the whole answer in
  one chunk (clients still get `finish_reason` and `[DONE]`);
- there are no tool calls and no reasoning stream, so the catalog reports
  `supportsToolCall: false` and `supportsReasoning: false`;
- one in-flight request per account, because the model setting is global.

The model catalog comes from `GET /v1/llm/models` (35 ids) — the OpenAI-shaped
`/v1/models` is a 404 there.

#### Upstream "soft refusal" and prompt sanitising

The upstream does not fail on input it does not want; it commits a normal
assistant message whose body is a fixed sentence:

```
我暂时无法处理这条消息，请稍后重试或换个话题。
```

("I can't handle this message right now — try again later or switch topics.")
That is a refusal, not an answer. It is easy to trigger: paste Cursor's
`<user_info>` (tens of KB, containing `<git_status>`, skill lists, the MCP tool
catalog) and `<rules>` (user rules, often 10K+ characters of "must / never"
imperatives) straight into `body.text` and it fires almost every time; plain
conversation history starts doing it around 40–50K characters.

The plugin handles it in three steps (`crates/providers-todayai/src/todayai/chat.rs`):

1. **sanitise** — drop client metadata blocks (`<git_status>`,
   `<agent_skills>`, `<mcp_meta_tools>`, `<open_and_recently_viewed_files>`, …),
   keep only the first 1.2K characters of `<rules>` / `<user_rule>` and the first
   600 of `<user_info>` (OS, shell and workspace path come first);
2. **budget** — cap the flattened prompt at 30K characters, giving the current
   question priority (12K) and filling history from newest to oldest at most 4K
   per turn; dropped history is announced as "earlier N messages omitted";
3. **retry, then fail loudly** — on refusal, retry once in a *fresh*
   conversation with a minimal prompt (last two turns only); if that is refused
   too, return an explicit error explaining why instead of dressing the refusal
   up as a reply.

### Standalone pack: OpenCode

`crates/providers-opencode` is a separate installable pack (`agent2api-opencode`)
containing **two** providers, because `stateful` is a per-provider static
capability and the two upstreams speak different protocols:

| id | upstream | status |
|---|---|---|
| `opencode-local` | local opencode desktop / `opencode serve` v2 API (`/api/*`) | **recommended**, verified working |
| `opencode` | `opencode.ai/inference` anonymous lane | quota-limited, may return 429 |

Build and pack:

```bash
cargo build --release -p agent2api-providers-opencode
node scripts/pack.mjs --pack crates/providers-opencode \
     --crate agent2api-providers-opencode
# → dist/agent2api-opencode-<version>-<target>.zip
```

**Why the local provider exists.** The anonymous cloud lane returns `GET
/v1/models` with 84 models but `POST /v1/chat/completions` answers
`429 FreeUsageLimitError` ("Free models work in OpenCode") with a ~14h
`Retry-After`; the quota is per egress IP, so rotating accounts/keys cannot
help. The same free models do answer through the **local opencode client's own
v2 API**. That API is not OpenAI-compatible — the server in OpenCode's
*Settings → Servers* page is a frontend + v2 REST API, and unknown paths fall
back to the SPA. Filling `http://127.0.0.1:<port>` into a "custom upstream
base URL" field therefore cannot work.

`opencode-local` is declared `stateful` and drives the session itself:

1. `GET /api/model` → catalog (one entry per `providerID/modelID` pair);
2. `GET /api/event` → subscribe to the global SSE stream first;
3. `POST /api/session` → create a session with `{model:{id,providerID}}`;
4. `POST /api/session/{id}/prompt` → `{text}` (messages are flattened into one
   labelled text, one session per request);
5. `session.text.delta` events → OpenAI `chat.completion.chunk` frames, then
   `finish_reason:"stop"` + `data: [DONE]`; if the stream fails it falls back
   to polling `/api/session/{id}/message` for the full answer;
6. `DELETE /api/session/{id}` → cleanup.

**Setup.** Start the opencode desktop app (or `opencode serve`), open the pack's
**OpenCode 账号** page and click "自动发现并连接". The plugin reads
`~/.local/state/opencode/service.json` for the URL and password (Basic auth,
user `opencode`). The port is randomly assigned on every start, so auto-discovered
credentials deliberately store **no** URL/password — they are re-read on each
request. You can also fill the URL/password manually. Prerequisite: the
OpenCode app must be running while requests are served.

**Limits.** v2 `prompt` accepts one text blob, so tool calls, structured output
and `logprobs` are not mapped; the reply is plain text conversation. SSE is a
global event stream, filtered by `sessionID`.

For the cloud provider, the model catalog now comes from the **same**
`inference/openai/v1/models` endpoint used for forwarding, filtered by zero
`cost.text.*.price` (12 free models). The old source (`models.opencode.ai/api.json`)
advertised 51 models, 39 of which that lane does not serve. A `429
FreeUsageLimitError` is classified `Fatal` (not quota-limited), since switching
accounts cannot recover an IP-based quota.

### Standalone pack: Meoo

`crates/providers-meoo` is a separate installable pack (`agent2api-meoo`)
containing one provider, `meoo`. Build and pack it on its own:

```bash
cargo build --release -p agent2api-providers-meoo
node scripts/pack.mjs --pack crates/providers-meoo \
     --crate agent2api-providers-meoo
# → dist/agent2api-meoo-<version>-<target>.zip
```

Add accounts from the pack's own **Meoo 秒悟** page (`ui/meoo/`). As with the
other standalone packs, the gateway's built-in "Add account" dialog only renders
its hardcoded provider table, so a third-party provider id shows "coming soon"
there.

**Two credentials, two planes.** Meoo does not use one token for everything:

| Plane | Base URL | Bearer | Used for |
| --- | --- | --- | --- |
| Control plane | `https://meoo.com/open/v1/cli-compat/…` | OAuth access token | identity, points, entitlement, projects, service AK |
| AI gateway | `https://api.meoo.host/meoo-ai/compatible-mode/v1` | project service AK (`meoo_project_service_v1.*`) | `/models`, `/chat/completions` |

An account carrying only a service AK can chat and refresh the model catalog,
but cannot read points or check in — the control plane rejects an AK as a
bearer token. Browser sign-in obtains both.

**Sign-in.** The button opens the official OAuth authorize page in the system
browser (authorization code + PKCE, `scope=cli.compat`, dynamic loopback
`redirect_uri`). The plugin binds `127.0.0.1:0` itself, receives the callback,
exchanges the code, reads the identity and tries to collect the project service
AK. No cookies are read and the user's everyday browser profile is untouched.
The page also offers a "paste a service AK" fallback.

> The plugin's own page runs inside the gateway webview / iframe, where
> `window.open` and `<a target="_blank">` are often blocked by the host (the
> flow keeps polling but no window ever appears). The **plugin process** therefore
> launches the system default browser itself — on Windows via
> `rundll32 url.dll,FileProtocolHandler`, without going through `cmd`, so the
> percent-encoded `redirect_uri` is not mangled by environment expansion. The
> page also shows the full authorize URL in plain text with a "reopen" action and
> a copy button. Set `MEOO_OPEN_BROWSER=0` for headless / automated runs.

**Check-in has no upstream endpoint.** The official docs
(`docs.meoo.com/coindesc`) say every user gets 2,000 points on their **first
login of the day**; there is no claim/task endpoint to call. So `checkin.claim`
does one token refresh (a login), touches `/account/identity`, then re-reads
`/account/points` up to twice and reports `Claimed` only when the balance
actually rose. An unchanged balance with a valid daily-login benefit reports
`Done` (already granted today); an unreadable balance reports `Failed`.
`claim.status` is strictly read-only and never triggers a login. This is the
same evidence discipline as the WPS Lingxi pack, minus the task-status recheck
there is no endpoint for.

`build_chat` is stateless: the plugin returns the gateway URL, headers and body
and lets the gateway send the request, so it never proxies an SSE stream.

## Every platform needs its own package

A plugin is a native executable that the gateway starts directly, so each
OS-and-architecture pair needs its own binary. `pack.json` declares five:

| Target triple | Covers |
|---|---|
| `x86_64-pc-windows-msvc` | Windows desktop (x64) |
| `x86_64-apple-darwin` | macOS on Intel |
| `aarch64-apple-darwin` | macOS on Apple Silicon |
| `x86_64-unknown-linux-gnu` | Linux desktop amd64, Docker amd64 |
| `aarch64-unknown-linux-gnu` | Docker arm64 |

The gateway checks that the current platform's entry point is in the package
and refuses one that lacks it. Update checks match the asset name
`<id>-<version>-<target>.zip` exactly, so the platform segment cannot be
dropped from the filename.

**Both Linux packages build against the bookworm generation.** The Docker
runtime image is `debian:bookworm-slim` (glibc 2.36). A binary built on a newer
distribution (Ubuntu 24.04, glibc 2.39) requires a newer glibc and fails inside
the container with a dynamic-linker error. CI builds both Linux targets on
Ubuntu 22.04 runners (glibc 2.35), which stays below the runtime image and
therefore runs on it.

## Signing (optional)

```bash
node scripts/sign.mjs keygen                                    # once
node scripts/sign.mjs sign --zip dist/agent2api-providers-*.zip
```

The private key goes to `~/.agent2api/plugin-signing-key.json` by default (or
wherever `--key` / `AGENT2API_PLUGIN_KEY` points) and **never** enters the
repository. Signing is ed25519 over the package contents — id, version,
protocol, target, providers, entry points and the entry binary's sha256 — not
over the zip bytes. After generating a key, put the printed public key into the
gateway's `OFFICIAL_PUBKEYS` (`plugin_host/install.rs`); otherwise the gateway
does not trust it and refuses the install as an invalid signature.

An unsigned package installs and works normally. The interface marks it
"unsigned" in amber, which only means the origin cannot be proven.

## Verifying

```bash
cargo build --release
node scripts/verify.mjs          # defaults to the binary under target/release
node scripts/verify.mjs --exe path/to/agent2api-providers
```

The script acts as the gateway and runs the protocol against the plugin binary:
init, requests, streaming, backpressure, cancellation, callbacks and crash
recovery. All 28 checks must pass.

Every CI platform job runs it — **compiling is not the same as running**. A
missing shared library, a missing executable bit or a wrong path only shows up
at startup, so each platform's binary is started once on its own machine
before the package is produced.

## The two shared crates

- `agent2api-provider-protocol` — the wire types (the contract); the gateway
  and every plugin compile it
- `agent2api-provider-sdk` — the `Provider` trait and the `serve()` loop

Both live under `crates/` here, and `crates/providers/Cargo.toml` refers to
them by relative path inside the workspace. A protocol change takes effect
locally without a release.

The gateway refers to the same repository **by tag**:

```toml
agent2api-provider-protocol = { git = "https://github.com/ProvidersPlugin/Agent2Providers", tag = "v0.2.1" }
```

So a contract change reaches the gateway only after a new tag and an updated
line there. Changing the local copy alone leaves the gateway compiling against
the old contract; a field mismatch may still compile and fail at runtime.

## License

AGPL-3.0
