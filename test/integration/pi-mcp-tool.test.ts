import test from 'node:test'
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { PiRpcProcess } from '../../src/pi-rpc/process.js'

const piCommand = process.env.PI_ACP_TEST_PI_COMMAND ?? 'pi'
let piVersion = ''
try {
  piVersion = execFileSync(piCommand, ['--version'], { encoding: 'utf8' }).trim()
} catch {
  // The test is reported as skipped when Pi is not installed in the test environment.
}

const versionMatch = piVersion.match(/(\d+)\.(\d+)\.(\d+)/)
const hasRequiredPi =
  versionMatch !== null &&
  (Number(versionMatch[1]) > 1 || (Number(versionMatch[1]) === 1 && Number(versionMatch[2]) >= 4))

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`
}

function writeMcpServer(root: string): string {
  const server = join(root, 'mcp-server.cjs')
  writeFileSync(
    server,
    [
      "const { appendFileSync } = require('node:fs')",
      "const readline = require('node:readline')",
      'const marker = process.env.PI_TEST_MCP_MARKER',
      'const literal = process.env.PI_TEST_LITERAL',
      "const tools = [{ name: 'echo', description: 'deterministic echo', inputSchema: { type: 'object', properties: { value: { type: 'string' } }, required: ['value'] } }]",
      "function reply(id, result) { process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id, result }) + '\\n') }",
      "readline.createInterface({ input: process.stdin }).on('line', line => {",
      '  const request = JSON.parse(line)',
      "  if (request.method === 'initialize') reply(request.id, { protocolVersion: '2024-11-05', capabilities: { tools: {} }, serverInfo: { name: 'fixture', version: '1.0.0' } })",
      "  else if (request.method === 'tools/list') reply(request.id, { tools })",
      "  else if (request.method === 'tools/call') { appendFileSync(marker, JSON.stringify({ params: request.params, literal }) + '\\n'); reply(request.id, { content: [{ type: 'text', text: String(request.params.arguments?.value ?? '') }], isError: false }) }",
      '})',
      'process.stdin.resume()'
    ].join('\n'),
    'utf8'
  )
  return server
}

function writeDeterministicProvider(root: string, marker: string): string {
  const extension = join(root, 'deterministic-provider.cjs')
  writeFileSync(
    extension,
    [
      'const zeroCost = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 }',
      'const zeroUsage = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: zeroCost }',
      'function stream() {',
      '  const queue = []',
      '  const waiters = []',
      '  let ended = false',
      '  let finalValue',
      '  const push = value => { if (waiters.length) waiters.shift()({ value, done: false }); else queue.push(value) }',
      '  const end = value => { ended = true; finalValue = value; while (waiters.length) waiters.shift()({ value: undefined, done: true }) }',
      '  const next = () => queue.length ? Promise.resolve({ value: queue.shift(), done: false }) : ended ? Promise.resolve({ value: undefined, done: true }) : new Promise(resolve => waiters.push(resolve))',
      '  return { push, end, result: async () => finalValue, [Symbol.asyncIterator]: () => ({ next }) }',
      '}',
      "function message(model, content, stopReason) { return { role: 'assistant', content, api: model.api, provider: model.provider, model: model.id, usage: zeroUsage, stopReason, timestamp: Date.now() } }",
      'function streamResponse(model, context) {',
      '  const output = stream()',
      '  Promise.resolve().then(() => {',
      "    const hasResult = context.messages.some(item => item.role === 'toolResult' && (item.toolName === 'mcp__tools__echo' || item.toolName === 'echo'))",
      "    const start = message(model, [], 'pending')",
      "    output.push({ type: 'start', partial: start })",
      '    if (hasResult) {',
      "      const text = { type: 'text', text: 'deterministic tool result received' }",
      "      output.push({ type: 'text_start', contentIndex: 0, partial: message(model, [text], 'pending') })",
      "      output.push({ type: 'text_delta', contentIndex: 0, delta: text.text, partial: message(model, [text], 'pending') })",
      "      const done = message(model, [text], 'stop')",
      "      output.push({ type: 'text_end', contentIndex: 0, content: text.text, partial: done })",
      "      output.push({ type: 'done', reason: 'stop', message: done })",
      '      output.end(done)',
      '      return',
      '    }',
      "    const call = { type: 'toolCall', id: 'fixture-call', name: 'mcp__tools__echo', arguments: { value: 'deterministic' } }",
      "    const partial = message(model, [call], 'toolUse')",
      "    output.push({ type: 'toolcall_start', contentIndex: 0, partial })",
      "    output.push({ type: 'toolcall_end', contentIndex: 0, toolCall: call, partial })",
      "    output.push({ type: 'done', reason: 'toolUse', message: partial })",
      '    output.end(partial)',
      '  })',
      '  return output',
      '}',
      'module.exports = function (pi) {',
      "  pi.registerProvider('fixture-provider', {",
      "    name: 'Fixture Provider', api: 'fixture-api', apiKey: 'fixture-key', baseUrl: 'http://127.0.0.1',",
      "    models: [{ id: 'deterministic', name: 'Deterministic', input: ['text'], reasoning: false, contextWindow: 4096, maxTokens: 256, cost: zeroCost }],",
      '    streamSimple: streamResponse',
      '  })',
      `void ${JSON.stringify(marker)}`,
      '}'
    ].join('\n'),
    'utf8'
  )
  return extension
}

test(
  'Pi native MCP calls a deterministic supplied tool through the ACP bridge',
  { skip: hasRequiredPi ? false : `requires Pi >=1.4.0 (found ${piVersion || 'no pi'})` },
  async () => {
    const root = mkdtempSync(join(tmpdir(), 'pi-acp-mcp-integration-'))
    const home = join(root, 'home')
    const project = join(root, 'project')
    mkdirSync(home)
    mkdirSync(project)
    const marker = join(root, 'calls.jsonl')
    const server = writeMcpServer(root)
    const provider = writeDeterministicProvider(root, marker)
    const wrapper = join(root, 'pi-wrapper')
    writeFileSync(
      wrapper,
      `#!/bin/sh\nexec ${shellQuote(piCommand)} --extension ${shellQuote(provider)} "$@"\n`,
      'utf8'
    )
    chmodSync(wrapper, 0o755)

    const previousHome = process.env.HOME
    process.env.HOME = home
    let proc: PiRpcProcess | undefined
    const events: any[] = []
    try {
      proc = await PiRpcProcess.spawn({
        cwd: project,
        piCommand: wrapper,
        sessionPath: join(project, 'session.jsonl'),
        mcpServers: [
          {
            name: 'tools',
            command: process.execPath,
            args: [server],
            env: [
              { name: 'PI_TEST_MCP_MARKER', value: marker },
              { name: 'PI_TEST_LITERAL', value: '${PI_ACP_LITERAL_DO_NOT_RESOLVE} !not-a-command' }
            ]
          }
        ] as any
      })
      const settled = new Promise<void>((resolve, reject) => {
        const timeout = setTimeout(() => reject(new Error('Pi prompt did not settle within 10 seconds')), 10_000)
        proc!.onEvent(event => {
          events.push(event)
          if (event.type === 'agent_end' || event.type === 'agent_settled') {
            clearTimeout(timeout)
            resolve()
          }
        })
      })
      await proc.setModel('fixture-provider', 'deterministic')
      await proc.prompt('call the deterministic tool')
      await settled

      const calls = readFileSync(marker, 'utf8')
        .trim()
        .split('\n')
        .filter(Boolean)
        .map(line => JSON.parse(line))
      assert.equal(calls.length, 1)
      assert.equal(calls[0].params.name, 'echo')
      assert.deepEqual(calls[0].params.arguments, { value: 'deterministic' })
      assert.equal(calls[0].literal, '${PI_ACP_LITERAL_DO_NOT_RESOLVE} !not-a-command')
    } finally {
      proc?.dispose()
      await new Promise(resolve => setTimeout(resolve, 100))
      if (previousHome === undefined) delete process.env.HOME
      else process.env.HOME = previousHome
      rmSync(root, { recursive: true, force: true })
    }
  }
)
