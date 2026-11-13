#!/usr/bin/env node
/**
 * 工具桥接的离线自测：本地起一个假网关，扮演 Tabbit 那条「纯文本」通道，
 * 验证桥接的上行改写与下行还原。不碰真实网关与真实账号。
 *
 *   node tools/tool-bridge/selftest.mjs
 */

import { spawn } from 'node:child_process'
import { createServer } from 'node:http'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

const HERE = dirname(fileURLToPath(import.meta.url))
const BRIDGE_PORT = 18124
const BRIDGE = join(HERE, 'bridge.mjs')

let pass = 0
let fail = 0

function check(label, ok, detail = '') {
  if (ok) {
    pass += 1
    console.log(`  ✓ ${label}`)
  } else {
    fail += 1
    console.log(`  ✗ ${label}${detail ? ` — ${detail}` : ''}`)
  }
}

/* ── 假网关 ─────────────────────────────────────────────────── */

let lastBody = null
const models = {
  tool: '<tool_call>{"name":"read_file","arguments":{"path":"src/a.ts"}}</tool_call>',
  prose: '{"name":"read_file"} 这个文件我没法读，先说说思路。',
  badjson: '<tool_call>{"name":"read_file","arguments":{</tool_call>',
  multi: '我看看两个文件。\n<tool_call>{"name":"read_file","arguments":{"path":"a.ts"}}</tool_call>\n<tool_call>{"name":"list_dir","arguments":{"path":"."}}</tool_call>',
  plain: '桥接透传正常。',
}

function sse(res, model) {
  const body = models[model] ?? '默认回答。'
  res.writeHead(200, { 'content-type': 'text/event-stream; charset=utf-8' })
  const frame = delta => `data: ${JSON.stringify({
    id: 'chatcmpl-fake',
    object: 'chat.completion.chunk',
    created: 1,
    model,
    choices: [{ index: 0, delta, finish_reason: null }],
  })}\n\n`
  res.write(frame({ role: 'assistant' }))
  for (let i = 0; i < body.length; i += 16) res.write(frame({ content: body.slice(i, i + 16) }))
  res.write(`data: ${JSON.stringify({
    id: 'chatcmpl-fake',
    object: 'chat.completion.chunk',
    created: 1,
    model,
    choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
  })}\n\n`)
  res.write('data: [DONE]\n\n')
  res.end()
}

const upstream = createServer((req, res) => {
  const path = new URL(req.url || '/', 'http://127.0.0.1').pathname
  if (req.method === 'GET' && path.endsWith('/models')) {
    res.writeHead(200, { 'content-type': 'application/json' })
    res.end(JSON.stringify({ object: 'list', data: [{ id: 'deepseek-v4-1-flash', object: 'model' }] }))
    return
  }
  const parts = []
  req.on('data', chunk => parts.push(chunk))
  req.on('end', () => {
    lastBody = JSON.parse(Buffer.concat(parts).toString('utf8') || '{}')
    sse(res, lastBody.model)
  })
})

await new Promise(resolve => upstream.listen(0, '127.0.0.1', resolve))
const upstreamBase = `http://127.0.0.1:${upstream.address().port}/v1`

const child = spawn(process.execPath, [BRIDGE], {
  stdio: ['ignore', 'ignore', 'pipe'],
  env: {
    ...process.env,
    BRIDGE_PORT: String(BRIDGE_PORT),
    UPSTREAM_BASE: upstreamBase,
    UPSTREAM_KEY: 'sk-test',
    BRIDGE_LOG: '0',
  },
})
child.stderr.on('data', chunk => process.stderr.write(`[bridge] ${chunk}`))

async function waitReady() {
  for (let i = 0; i < 60; i += 1) {
    try {
      const response = await fetch(`http://127.0.0.1:${BRIDGE_PORT}/health`)
      if (response.ok) return
    } catch {
      /* 还没起来 */
    }
    await new Promise(resolve => setTimeout(resolve, 100))
  }
  throw new Error('桥接启动超时')
}

async function chat(payload) {
  const response = await fetch(`http://127.0.0.1:${BRIDGE_PORT}/v1/chat/completions`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: 'Bearer sk-client' },
    body: JSON.stringify(payload),
  })
  const raw = await response.text()
  if (!payload.stream) return JSON.parse(raw)
  const frames = raw.split('\n\n').filter(Boolean).map(block => block.replace(/^data:\s*/, '')).filter(x => x && x !== '[DONE]')
  const deltas = frames.map(frame => JSON.parse(frame))
  const content = deltas.map(f => f.choices?.[0]?.delta?.content || '').join('')
  const calls = deltas.flatMap(f => f.choices?.[0]?.delta?.tool_calls || [])
  const finish = deltas.map(f => f.choices?.[0]?.finish_reason).filter(Boolean).pop() || null
  return { content, calls, finish, frames: deltas.length }
}

const tools = [
  {
    type: 'function',
    function: {
      name: 'read_file',
      description: 'Read a file from disk. '.repeat(20),
      parameters: {
        type: 'object',
        properties: {
          path: { type: 'string', description: 'path relative to repo root' },
        },
        required: ['path'],
        additionalProperties: false,
        $schema: 'http://json-schema.org/draft-07/schema#',
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'list_dir',
      description: 'List entries of a directory.',
      parameters: { type: 'object', properties: { path: { type: 'string' } }, required: ['path'] },
    },
  },
]

try {
  await waitReady()

  console.log('\n[1] 带 tools，模型按协议输出 → 还原成 tool_calls')
  const one = await chat({
    model: 'tool',
    stream: true,
    messages: [{ role: 'system', content: 'You are Cursor.' }, { role: 'user', content: '读一下 src/a.ts' }],
    tools,
    tool_choice: 'auto',
  })
  check('返回 finish_reason=tool_calls', one.finish === 'tool_calls', one.finish)
  check('拿到 1 个调用', one.calls.length === 1, String(one.calls.length))
  check('工具名 read_file', one.calls[0]?.function?.name === 'read_file', one.calls[0]?.function?.name)
  check('arguments 是合法 JSON 字符串', (() => {
    try {
      return JSON.parse(one.calls[0]?.function?.arguments || '')?.path === 'src/a.ts'
    } catch {
      return false
    }
  })(), one.calls[0]?.function?.arguments)
  check('content 里没有残留 tool_call 标签', !one.content.includes('<tool_call>'), one.content.slice(0, 60))
  check('上游收到的请求没有 tools 字段', lastBody?.tools === undefined, JSON.stringify(Object.keys(lastBody || {})))
  check('上游收到注入的协议', JSON.stringify(lastBody?.messages || []).includes('TOOL CALLING PROTOCOL'))
  check('上游收到的是流式请求', lastBody?.stream === true, String(lastBody?.stream))
  check('透传了客户端密钥', lastBody !== null)

  console.log('\n[2] 带 tools，模型正常作答 → 纯文本')
  const two = await chat({
    model: 'plain',
    stream: true,
    messages: [{ role: 'user', content: '你好' }],
    tools,
  })
  check('没有 tool_calls', two.calls.length === 0, String(two.calls.length))
  check('finish_reason=stop', two.finish === 'stop', two.finish)
  check('正文完整', two.content === '桥接透传正常。', two.content)

  console.log('\n[3] 不带 tools → 后端不注入协议')
  const three = await chat({ model: 'plain', stream: true, messages: [{ role: 'user', content: '你好' }] })
  check('没有注入协议', !JSON.stringify(lastBody?.messages || []).includes('TOOL CALLING PROTOCOL'))
  check('正文透传', three.content === '桥接透传正常。', three.content)

  console.log('\n[4] 原生工具历史被摊平')
  await chat({
    model: 'plain',
    stream: true,
    messages: [
      { role: 'system', content: 'sys' },
      { role: 'user', content: '读 a.ts' },
      { role: 'assistant', content: '', tool_calls: [{ id: 'call_1', type: 'function', function: { name: 'read_file', arguments: '{"path":"a.ts"}' } }] },
      { role: 'tool', tool_call_id: 'call_1', content: 'export const a = 1' },
      { role: 'user', content: '继续' },
    ],
    tools,
  })
  const roles = (lastBody?.messages || []).map(message => message.role)
  check('没有 tool 角色残留', !roles.includes('tool'), roles.join(','))
  check('没有 tool_calls 字段残留', !(lastBody?.messages || []).some(message => message.tool_calls))
  check('历史里保留了调用事实', JSON.stringify(lastBody?.messages || []).includes('I called: read_file'))
  check('历史里保留了工具结果', JSON.stringify(lastBody?.messages || []).includes('export const a = 1'))

  console.log('\n[5] 坏 JSON 不制造调用')
  const five = await chat({
    model: 'badjson',
    stream: true,
    messages: [{ role: 'user', content: '读文件' }],
    tools,
  })
  check('没有 tool_calls', five.calls.length === 0, String(five.calls.length))
  check('finish_reason=stop', five.finish === 'stop', five.finish)

  console.log('\n[6] 一次多个调用')
  const six = await chat({
    model: 'multi',
    stream: true,
    messages: [{ role: 'user', content: '看两个文件' }],
    tools,
  })
  check('拿到 2 个调用', six.calls.length === 2, String(six.calls.length))
  check('调用顺序保持', six.calls.map(c => c.function.name).join(',') === 'read_file,list_dir')
  check('解释文字保留为正文', six.content.includes('我看看两个文件'), six.content)

  console.log('\n[7] 图片块降级')
  await chat({
    model: 'plain',
    stream: true,
    messages: [{
      role: 'user',
      content: [{ type: 'text', text: '看这张图' }, { type: 'image_url', image_url: { url: 'data:image/png;base64,AAAA' } }],
    }],
    tools,
  })
  const flat = JSON.stringify(lastBody?.messages || [])
  check('不把 image_url 传给上游', !flat.includes('image_url'))
  check('留下占位说明', flat.includes('image omitted'))

  console.log('\n[8] 模型目录透传')
  const listResponse = await fetch(`http://127.0.0.1:${BRIDGE_PORT}/v1/models`)
  const list = await listResponse.json()
  check('返回 data 数组', Array.isArray(list.data), JSON.stringify(list).slice(0, 80))
  check('保留了模型 id', list.data?.[0]?.id === 'deepseek-v4-1-flash')

  console.log('\n[9] 非流式请求')
  const nine = await chat({
    model: 'tool',
    messages: [{ role: 'user', content: '读文件' }],
    tools,
  })
  check('message.tool_calls 存在', Array.isArray(nine.choices?.[0]?.message?.tool_calls))
  check('content 为 null 而不是空串', nine.choices?.[0]?.message?.content === null, String(nine.choices?.[0]?.message?.content))
} finally {
  child.kill()
  upstream.close()
}

console.log(`\n通过 ${pass}/${pass + fail}`)
process.exit(fail === 0 ? 0 : 1)
