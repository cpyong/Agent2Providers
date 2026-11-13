#!/usr/bin/env node
/**
 * 工具桥接：让「没有原生 tool calling」的提供商（Tabbit、DeepSeek 网页……）
 * 也能被 Cursor 这类 Agent 客户端使用。
 *
 * ── 为什么需要它 ────────────────────────────────────────────
 * Tabbit 网页走 v3 房间协议，`agent_input` 里只有 `text` / `files` / `refs`，
 * 没有 tools 字段；官方插件因此在收到 tools 时直接返回 400：
 *
 *   Tabbit 网页没有原生工具调用接口，请移除 tools / functions，或改用支持工具调用的提供商
 *
 * Cursor 的 Agent 模式没有「关掉工具」的开关 —— 读文件、改文件、跑命令
 * 全靠 function calling，它会无条件带 tools。于是「测试能通、一用就报错」。
 *
 * 这一层夹在中间：
 *
 *   Cursor ──► 本进程 ──► Agent2API 网关 ──► Tabbit
 *            （拆 tools）              （纯文本，上游原生链路）
 *
 * 上行把 tools 压成一段提示词协议，下行把模型吐出的约定格式还原成标准
 * `tool_calls`。对 Cursor 来说，这就是一个会调用工具的普通模型。
 *
 * ── 它不是什么 ──────────────────────────────────────────────
 * 不是「让网页端真的支持 function calling」。模型是在**文本**层面被要求
 * 输出一个 JSON，可靠性取决于模型的遵从度：模型可能不调用、调错参数、
 * 或把 JSON 写坏。桥接对坏 JSON 的处理是**丢弃该次调用**（当普通文本
 * 回答），绝不伪造一个可能破坏文件的调用。
 *
 * ── 运行 ────────────────────────────────────────────────────
 *   node tools/tool-bridge/bridge.mjs
 *
 * 环境变量：
 *   BRIDGE_PORT     本进程监听端口，默认 8124
 *   UPSTREAM_BASE   网关 OpenAI 兼容根地址，默认 http://127.0.0.1:3065/v1
 *   UPSTREAM_KEY    上游密钥；不填则原样透传客户端的 Authorization
 *   BRIDGE_LOG      设为 0 关闭请求日志
 *
 * Cursor 侧：Settings → Models → OpenAI API Key 填网关密钥，
 * Override OpenAI Base URL 填 http://127.0.0.1:8124/v1，模型名填 Tabbit 的
 * 模型 id（例如 deepseek-v4-1-flash）。
 */

import { createServer } from 'node:http'

const PORT = Number(process.env.BRIDGE_PORT || 8124)
const UPSTREAM_BASE = (process.env.UPSTREAM_BASE || 'http://127.0.0.1:3065/v1').replace(/\/+$/, '')
const UPSTREAM_KEY = process.env.UPSTREAM_KEY || ''
const LOG_ON = process.env.BRIDGE_LOG !== '0'

/** 上游是网页通道，出字慢；但也不能无限等。 */
const UPSTREAM_TIMEOUT_MS = 600_000

/** 单个工具描述与 schema 的裁剪上限：注入太长会挤掉 Cursor 自己的上下文。 */
const TOOL_DESC_LIMIT = 240
const TOOL_SCHEMA_DEPTH = 6

const log = (...args) => {
  if (LOG_ON) console.error('[bridge]', ...args)
}

/* ── 提示词协议 ─────────────────────────────────────────────── */

/**
 * 工具调用协议。用英文写，因为 Cursor 自己的 system prompt 是英文，
 * 混排中英会让模型在「用哪种语言思考」上摇摆，进而影响 JSON 的稳定性。
 */
function protocolBlock(tools) {
  return [
    '',
    '',
    '===== TOOL CALLING PROTOCOL (highest priority) =====',
    'You cannot call tools natively in this session. Tools are invoked through a text protocol.',
    'When you need to use a tool, output ONLY one or more blocks in exactly this shape:',
    '<tool_call>{"name":"<tool_name>","arguments":{<json object matching the schema>}}</tool_call>',
    '',
    'Rules:',
    '1. Always wrap the JSON in <tool_call></tool_call>. Emit raw JSON with no markdown fence.',
    '2. "arguments" MUST be a JSON object (not a string), following the tool schema exactly.',
    '3. Emit one <tool_call> block per tool call, in the order they should run.',
    '4. You may write a short sentence before the blocks; never write anything inside them except the JSON.',
    '5. When no tool is needed, answer normally and do NOT emit any <tool_call> block.',
    '6. Never invent tool names. Only the tools listed below exist.',
    '',
    'Available tools:',
    ...tools.map(renderTool),
    '===== END TOOL CALLING PROTOCOL =====',
  ].join('\n')
}

function renderTool(tool) {
  const fn = tool?.function || tool || {}
  const name = typeof fn.name === 'string' ? fn.name : ''
  const desc = clip(String(fn.description || '').replace(/\s+/g, ' '), TOOL_DESC_LIMIT)
  const schema = JSON.stringify(compressSchema(fn.parameters, 0))
  return `- ${name}: ${desc}\n  arguments schema: ${schema}`
}

function clip(text, limit) {
  return text.length > limit ? `${text.slice(0, limit)}…` : text
}

/**
 * 裁剪 JSON Schema。Cursor 的工具 schema 里塞了大量 `description` 与
 * `additionalProperties`，对网页通道没意义，还会把提示词顶到上游的截断线。
 * 这里只保留「模型必须知道才写得出参数」的部分：类型、属性名、必填、枚举。
 */
function compressSchema(schema, depth) {
  if (!schema || typeof schema !== 'object' || Array.isArray(schema)) return schema ?? {}
  if (depth >= TOOL_SCHEMA_DEPTH) return { type: schema.type || 'object' }

  const keep = ['type', 'enum', 'required', 'items', 'properties', 'anyOf', 'oneOf', 'const', 'default']
  const out = {}
  for (const key of keep) {
    if (!(key in schema)) continue
    const value = schema[key]
    if (key === 'properties' && value && typeof value === 'object') {
      out.properties = {}
      for (const [prop, sub] of Object.entries(value)) {
        out.properties[prop] = compressSchema(sub, depth + 1)
        const desc = sub && typeof sub.description === 'string' ? sub.description.trim() : ''
        if (desc) out.properties[prop].description = clip(desc, 120)
      }
    } else if (key === 'items') {
      out.items = Array.isArray(value) ? value.map(item => compressSchema(item, depth + 1)) : compressSchema(value, depth + 1)
    } else if (key === 'anyOf' || key === 'oneOf') {
      out[key] = Array.isArray(value) ? value.map(item => compressSchema(item, depth + 1)) : value
    } else {
      out[key] = value
    }
  }
  if (!out.type) out.type = schema.type || 'object'
  return out
}

/* ── 上行改写 ───────────────────────────────────────────────── */

/**
 * 把一条消息的 content 摊平成纯文本。
 * Tabbit 只收一段文本，图片块会换来另一条 400（"仅支持文本"），
 * 所以这里直接降级成占位符，不让它变成又一次失败的调用。
 */
function textOf(content) {
  if (content == null) return ''
  if (typeof content === 'string') return content
  if (!Array.isArray(content)) return String(content)
  const parts = []
  for (const block of content) {
    if (!block || typeof block !== 'object') continue
    if (block.type === 'text' || typeof block.text === 'string') parts.push(String(block.text || ''))
    else if (block.type === 'image_url' || block.type === 'image') parts.push('[image omitted: this channel accepts text only]')
    else if (block.type === 'input_audio' || block.type === 'audio') parts.push('[audio omitted: text only]')
  }
  return parts.join('\n')
}

/**
 * 历史里的 `assistant.tool_calls` / `role:"tool"` 是原生工具对话的形状，
 * 网页通道既不认识也不接受。这里把它们改写成人类可读的文本，保留
 * 「调过什么、拿到什么」这个事实，让模型在下一轮还能接着推理。
 */
function flattenToolHistory(messages) {
  const nameByCallId = new Map()
  const out = []
  for (const message of messages) {
    const role = message?.role
    if (role === 'assistant' && Array.isArray(message.tool_calls) && message.tool_calls.length > 0) {
      const calls = []
      for (const call of message.tool_calls) {
        const name = call?.function?.name || call?.name || 'unknown_tool'
        const id = call?.id || ''
        if (id) nameByCallId.set(id, name)
        const raw = call?.function?.arguments ?? call?.arguments ?? ''
        calls.push(`${name}(${typeof raw === 'string' ? raw : JSON.stringify(raw)})`)
      }
      const text = textOf(message.content)
      const suffix = `[I called: ${calls.join(', ')}]`
      out.push({ ...message, role: 'assistant', content: text ? `${text}\n\n${suffix}` : suffix, tool_calls: undefined })
      continue
    }
    if (role === 'tool' || role === 'function') {
      const name = nameByCallId.get(message.tool_call_id || '') || message.name || 'tool'
      const body = clip(textOf(message.content), 8000)
      out.push({ role: 'user', content: `[result of ${name}]\n${body}` })
      continue
    }
    out.push({ ...message, content: textOf(message.content) })
  }
  return out
}

/**
 * 带 tools 的请求：注入协议、删掉原生工具字段、摊平工具历史。
 * 返回改写后的 body 与工具名清单（用于下行校验）。
 */
function rewriteRequest(body) {
  const rawTools = Array.isArray(body.tools) ? body.tools : Array.isArray(body.functions) ? body.functions.map(fn => ({ type: 'function', function: fn })) : []
  const named = rawTools.filter(tool => {
    const name = tool?.function?.name || tool?.name
    return typeof name === 'string' && name.length > 0
  })

  let messages = flattenToolHistory(Array.isArray(body.messages) ? body.messages : [])

  if (named.length > 0) {
    const block = protocolBlock(named)
    const first = messages[0]
    if (first && (first.role === 'system' || first.role === 'developer')) {
      messages = [{ ...first, content: `${first.content}${block}` }, ...messages.slice(1)]
    } else {
      messages = [{ role: 'system', content: block.trimStart() }, ...messages]
    }
  }

  const next = { ...body, messages }
  delete next.tools
  delete next.functions
  delete next.tool_choice
  delete next.function_call
  delete next.parallel_tool_calls
  // 上游不支持这些扩展字段，留着只会换来一次 400。
  delete next.stream_options
  delete next.response_format
  return { body: next, tools: named.map(tool => tool.function?.name || tool.name) }
}

/* ── 下行解析 ───────────────────────────────────────────────── */

/** 从 `start` 位置的 `{` 起，找配对的 `}`，跳过字符串里的括号与转义。 */
function balancedJson(text, start) {
  if (text[start] !== '{') return null
  let depth = 0
  let inString = false
  let escaped = false
  for (let i = start; i < text.length; i += 1) {
    const ch = text[i]
    if (inString) {
      if (escaped) escaped = false
      else if (ch === '\\') escaped = true
      else if (ch === '"') inString = false
      continue
    }
    if (ch === '"') inString = true
    else if (ch === '{') depth += 1
    else if (ch === '}') {
      depth -= 1
      if (depth === 0) return text.slice(start, i + 1)
    }
  }
  return null
}

function normaliseCall(candidate, allowed) {
  if (!candidate || typeof candidate !== 'object') return null
  const fn = candidate.function && typeof candidate.function === 'object' ? candidate.function : candidate
  const name = typeof fn.name === 'string' ? fn.name.trim() : ''
  if (!name) return null
  if (allowed.length > 0 && !allowed.includes(name)) return null

  let args = fn.arguments ?? fn.params ?? fn.parameters ?? {}
  let serialised
  if (typeof args === 'string') {
    const trimmed = args.trim()
    if (!trimmed) serialised = '{}'
    else {
      try {
        const parsed = JSON.parse(trimmed)
        serialised = JSON.stringify(parsed ?? {})
      } catch {
        return null
      }
    }
  } else if (args && typeof args === 'object') {
    serialised = JSON.stringify(args)
  } else {
    serialised = '{}'
  }
  return { name, arguments: serialised }
}

/**
 * 从模型输出里抽取工具调用。
 * 依次尝试：<tool_call> 块 → ```json 代码块 → 整段就是 JSON → 文末最后一个
 * 平衡 JSON 对象。全部落空就把整段当普通回答 —— 宁可少调一次，也不猜。
 */
function parseToolCalls(text, allowed) {
  const source = String(text || '')
  const calls = []
  let consumed = source

  const tagPattern = /<tool_call>\s*([\s\S]*?)\s*<\/tool_call>/gi
  for (const match of source.matchAll(tagPattern)) {
    const inner = match[1].trim()
    const json = inner.startsWith('{') ? inner : balancedJson(inner, inner.indexOf('{'))
    if (!json) continue
    let parsed
    try {
      parsed = JSON.parse(json)
    } catch {
      continue
    }
    for (const item of Array.isArray(parsed) ? parsed : [parsed]) {
      const call = normaliseCall(item, allowed)
      if (call) calls.push(call)
    }
    consumed = consumed.replace(match[0], '')
  }
  if (calls.length > 0) return { calls, text: consumed.trim() }

  const fencePattern = /```(?:json)?\s*([\s\S]*?)```/gi
  for (const match of source.matchAll(fencePattern)) {
    const inner = match[1].trim()
    if (!inner.startsWith('{') && !inner.startsWith('[')) continue
    let parsed
    try {
      parsed = JSON.parse(inner)
    } catch {
      continue
    }
    const items = Array.isArray(parsed) ? parsed : Array.isArray(parsed?.tool_calls) ? parsed.tool_calls : [parsed]
    for (const item of items) {
      const call = normaliseCall(item, allowed)
      if (call) calls.push(call)
    }
    if (calls.length > 0) consumed = consumed.replace(match[0], '')
  }
  if (calls.length > 0) return { calls, text: consumed.trim() }

  const whole = source.trim()
  if (whole.startsWith('{')) {
    try {
      const parsed = JSON.parse(whole)
      const items = Array.isArray(parsed) ? parsed : Array.isArray(parsed?.tool_calls) ? parsed.tool_calls : [parsed]
      for (const item of items) {
        const call = normaliseCall(item, allowed)
        if (call) calls.push(call)
      }
      if (calls.length > 0) return { calls, text: '' }
    } catch {
      /* 不是合法 JSON，继续找文中对象 */
    }
  }

  // 模型常在 JSON 前面写一句「我来读一下这个文件」。
  for (let i = source.lastIndexOf('{'); i >= 0; i = source.lastIndexOf('{', i - 1)) {
    const json = balancedJson(source, i)
    if (!json) continue
    let parsed
    try {
      parsed = JSON.parse(json)
    } catch {
      continue
    }
    const items = Array.isArray(parsed) ? parsed : Array.isArray(parsed?.tool_calls) ? parsed.tool_calls : [parsed]
    const found = []
    for (const item of items) {
      const call = normaliseCall(item, allowed)
      if (call) found.push(call)
    }
    if (found.length > 0) return { calls: found, text: source.slice(0, i).trim() }
  }

  return { calls: [], text: whole }
}

/* ── 上游交互 ───────────────────────────────────────────────── */

/**
 * 把上游 SSE 收成一段完整结果。
 * 这里刻意不用非流式请求：网关对上游本来就是流式的，走同一条路更稳，
 * 聚合发生在本地，代价只是「工具轮次晚几百毫秒出字」。
 */
async function collectUpstream(response) {
  const contentType = response.headers.get('content-type') || ''
  if (!contentType.includes('text/event-stream')) {
    const payload = await response.json().catch(() => null)
    const choice = payload?.choices?.[0]
    return {
      text: textOf(choice?.message?.content),
      reasoning: textOf(choice?.message?.reasoning_content),
      nativeCalls: Array.isArray(choice?.message?.tool_calls) ? choice.message.tool_calls : [],
      finish: choice?.finish_reason || null,
      usage: payload?.usage || null,
    }
  }

  const decoder = new TextDecoder()
  let buffer = ''
  let text = ''
  let reasoning = ''
  let finish = null
  let usage = null
  const nativeCalls = []

  const handleLine = line => {
    const trimmed = line.trim()
    if (!trimmed.startsWith('data:')) return
    const data = trimmed.slice(5).trim()
    if (!data || data === '[DONE]') return
    let frame
    try {
      frame = JSON.parse(data)
    } catch {
      return
    }
    if (frame.usage) usage = frame.usage
    const choice = frame.choices?.[0]
    if (!choice) return
    const delta = choice.delta || choice.message || {}
    if (typeof delta.content === 'string') text += delta.content
    else if (Array.isArray(delta.content)) text += textOf(delta.content)
    if (typeof delta.reasoning_content === 'string') reasoning += delta.reasoning_content
    if (Array.isArray(delta.tool_calls)) {
      for (const partial of delta.tool_calls) {
        const index = Number.isInteger(partial.index) ? partial.index : nativeCalls.length
        const slot = nativeCalls[index] || (nativeCalls[index] = { id: '', name: '', arguments: '' })
        if (partial.id) slot.id = partial.id
        if (partial.function?.name) slot.name += partial.function.name
        if (partial.function?.arguments) slot.arguments += partial.function.arguments
      }
    }
    if (choice.finish_reason) finish = choice.finish_reason
  }

  for await (const chunk of response.body) {
    buffer += decoder.decode(chunk, { stream: true })
    let at
    while ((at = buffer.indexOf('\n')) >= 0) {
      handleLine(buffer.slice(0, at))
      buffer = buffer.slice(at + 1)
    }
  }
  buffer += decoder.decode()
  if (buffer.trim()) handleLine(buffer)

  return { text, reasoning, nativeCalls: nativeCalls.filter(Boolean), finish, usage }
}

function upstreamUrl(pathname) {
  const suffix = pathname.replace(/^\/v1/, '')
  return `${UPSTREAM_BASE}${suffix || ''}`
}

function forwardHeaders(headers) {
  const next = {}
  for (const [key, value] of Object.entries(headers)) {
    const lower = key.toLowerCase()
    if (['host', 'content-length', 'connection', 'accept-encoding'].includes(lower)) continue
    next[key] = value
  }
  if (UPSTREAM_KEY) {
    next.authorization = `Bearer ${UPSTREAM_KEY}`
    delete next['x-api-key']
  }
  return next
}

/* ── 下行输出 ───────────────────────────────────────────────── */

const newId = () => `chatcmpl-bridge-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 8)}`

function sseChunk(id, model, created, delta, finishReason = null) {
  return `data: ${JSON.stringify({
    id,
    object: 'chat.completion.chunk',
    created,
    model,
    choices: [{ index: 0, delta, finish_reason: finishReason }],
  })}\n\n`
}

/** 把整段文本切成小片下发，保持客户端的「逐字」观感。 */
function contentChunks(text, size = 48) {
  const pieces = []
  for (let i = 0; i < text.length; i += size) pieces.push(text.slice(i, i + size))
  return pieces
}

function writeStream(res, { model, text, calls, usage }) {
  const id = newId()
  const created = Math.floor(Date.now() / 1000)
  const finish = calls.length > 0 ? 'tool_calls' : 'stop'

  res.writeHead(200, {
    'content-type': 'text/event-stream; charset=utf-8',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
  })
  res.write(sseChunk(id, model, created, { role: 'assistant' }))

  for (const piece of contentChunks(text)) {
    res.write(sseChunk(id, model, created, { content: piece }))
  }
  calls.forEach((call, index) => {
    res.write(sseChunk(id, model, created, {
      tool_calls: [{
        index,
        id: call.id,
        type: 'function',
        function: { name: call.name, arguments: call.arguments },
      }],
    }))
  })
  res.write(sseChunk(id, model, created, {}, finish))
  if (usage) res.write(`data: ${JSON.stringify({ id, object: 'chat.completion.chunk', created, model, choices: [], usage })}\n\n`)
  res.write('data: [DONE]\n\n')
  res.end()
}

function writeJson(res, { model, text, calls, usage, reasoning }) {
  const message = { role: 'assistant', content: text || null }
  if (reasoning) message.reasoning_content = reasoning
  if (calls.length > 0) {
    message.tool_calls = calls.map(call => ({
      id: call.id,
      type: 'function',
      function: { name: call.name, arguments: call.arguments },
    }))
  }
  const payload = {
    id: newId(),
    object: 'chat.completion',
    created: Math.floor(Date.now() / 1000),
    model,
    choices: [{
      index: 0,
      message,
      finish_reason: calls.length > 0 ? 'tool_calls' : 'stop',
    }],
  }
  if (usage) payload.usage = usage
  res.writeHead(200, { 'content-type': 'application/json; charset=utf-8' })
  res.end(JSON.stringify(payload))
}

/* ── 请求处理 ───────────────────────────────────────────────── */

function readBody(req) {
  return new Promise((resolve, reject) => {
    const parts = []
    req.on('data', chunk => parts.push(chunk))
    req.on('end', () => resolve(Buffer.concat(parts).toString('utf8')))
    req.on('error', reject)
  })
}

function fail(res, status, message) {
  log('reject', status, message)
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8' })
  res.end(JSON.stringify({ error: { message, type: 'bridge_error' } }))
}

async function handleChat(req, res) {
  const raw = await readBody(req)
  let body
  try {
    body = JSON.parse(raw)
  } catch {
    return fail(res, 400, '工具桥接：请求体不是合法 JSON')
  }

  const { body: outbound, tools } = rewriteRequest(body)
  const wantsStream = body.stream === true
  const model = String(body.model || 'unknown')

  log(`→ ${model} | tools=${tools.length} | messages=${outbound.messages.length} | stream=${wantsStream}`)

  let response
  try {
    response = await fetch(upstreamUrl('/v1/chat/completions'), {
      method: 'POST',
      headers: { ...forwardHeaders(req.headers), 'content-type': 'application/json' },
      body: JSON.stringify({ ...outbound, stream: true }),
      signal: AbortSignal.timeout(UPSTREAM_TIMEOUT_MS),
    })
  } catch (error) {
    return fail(res, 502, `工具桥接：连不上上游 ${UPSTREAM_BASE}（${error?.message || error}）`)
  }

  if (!response.ok) {
    const detail = await response.text().catch(() => '')
    log('upstream', response.status, detail.slice(0, 300))
    res.writeHead(response.status, { 'content-type': 'application/json; charset=utf-8' })
    res.end(detail || JSON.stringify({ error: { message: `上游返回 ${response.status}` } }))
    return
  }

  const collected = await collectUpstream(response)

  // 上游若真的给了原生 tool_calls（换到支持工具的提供商时会发生），原样透传。
  let calls = collected.nativeCalls.map(call => ({
    id: call.id || `call_${Math.random().toString(36).slice(2, 10)}`,
    name: call.name,
    arguments: call.arguments || '{}',
  }))
  let text = collected.text
  if (calls.length === 0 && tools.length > 0) {
    const parsed = parseToolCalls(text, tools)
    calls = parsed.calls.map(call => ({ id: `call_${Math.random().toString(36).slice(2, 10)}`, ...call }))
    text = parsed.text
  }

  log(`← ${model} | text=${text.length} | calls=${calls.map(call => call.name).join(',') || '-'} | finish=${collected.finish || '-'}`)

  const payload = { model, text, calls, usage: collected.usage, reasoning: collected.reasoning }
  if (wantsStream) writeStream(res, payload)
  else writeJson(res, payload)
}

async function proxyGet(req, res, pathname) {
  try {
    const response = await fetch(upstreamUrl(pathname), {
      method: 'GET',
      headers: forwardHeaders(req.headers),
      signal: AbortSignal.timeout(30_000),
    })
    const buffer = Buffer.from(await response.arrayBuffer())
    res.writeHead(response.status, { 'content-type': response.headers.get('content-type') || 'application/json' })
    res.end(buffer)
  } catch (error) {
    fail(res, 502, `工具桥接：上游不可达（${error?.message || error}）`)
  }
}

const server = createServer(async (req, res) => {
  const pathname = new URL(req.url || '/', `http://127.0.0.1:${PORT}`).pathname
  try {
    if (pathname === '/health') {
      res.writeHead(200, { 'content-type': 'application/json' })
      res.end(JSON.stringify({ ok: true, upstream: UPSTREAM_BASE, port: PORT }))
      return
    }
    if (req.method === 'POST' && pathname.endsWith('/chat/completions')) {
      await handleChat(req, res)
      return
    }
    if (req.method === 'GET') {
      await proxyGet(req, res, pathname)
      return
    }
    fail(res, 404, `工具桥接：没有这个路由 ${req.method} ${pathname}`)
  } catch (error) {
    log('error', error?.stack || error)
    if (!res.headersSent) fail(res, 500, `工具桥接内部错误：${error?.message || error}`)
    else res.end()
  }
})

server.listen(PORT, '127.0.0.1', () => {
  log(`listening on http://127.0.0.1:${PORT}/v1 → ${UPSTREAM_BASE}`)
})
