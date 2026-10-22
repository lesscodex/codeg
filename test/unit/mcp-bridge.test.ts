import test from 'node:test'
import assert from 'node:assert/strict'
import { existsSync, readFileSync, statSync } from 'node:fs'
import { McpBridgeError, createMcpBridge, mapMcpServers } from '../../src/mcp/mcp-bridge.js'

const cwd = '/tmp/pi-acp-mcp-project'

test('MCP bridge maps ACP stdio and streamable HTTP transports', () => {
  const mapped = mapMcpServers(
    [
      {
        name: 'local-tools',
        command: 'node',
        args: ['server.js', '--flag with spaces'],
        env: [{ name: 'TOKEN', value: 'secret' }]
      },
      {
        type: 'http',
        name: 'remote_tools',
        url: 'https://example.test/mcp',
        headers: [{ name: 'Authorization', value: 'Bearer test' }]
      } as any
    ] as any,
    cwd
  )

  assert.deepEqual(mapped, {
    'local-tools': {
      exposure: 'direct',
      command: 'node',
      args: ['server.js', '--flag with spaces'],
      env: { TOKEN: 'secret' },
      cwd
    },
    remote_tools: {
      exposure: 'direct',
      url: 'https://example.test/mcp',
      headers: { Authorization: 'Bearer test' }
    }
  })
})

test('MCP bridge rejects unsupported transports and Pi name collisions', () => {
  assert.throws(
    () => mapMcpServers([{ type: 'sse', name: 'legacy', url: 'https://example.test/sse', headers: [] }] as any, cwd),
    (error: unknown) =>
      error instanceof McpBridgeError && error.code === 'unsupported' && error.message.includes('legacy SSE')
  )
  assert.throws(() => mapMcpServers([{ type: 'acp', name: 'nested', id: 'acp-1' }] as any, cwd), /ACP transport/)
  assert.throws(
    () => mapMcpServers([{ type: 'http', name: 'missing-url', command: 'server' }] as any, cwd),
    /needs a url/
  )

  assert.throws(
    () =>
      mapMcpServers(
        [
          { name: 'same-name', command: 'one', args: [], env: [] },
          { name: 'same_name', command: 'two', args: [], env: [] }
        ] as any,
        cwd
      ),
    /name collision/
  )
})

test('MCP bridge keeps per-session config ephemeral and cleans it up', () => {
  const first = createMcpBridge([{ name: 'one', command: 'one', args: [], env: [] }] as any, cwd)
  const second = createMcpBridge([{ name: 'two', command: 'two', args: [], env: [] }] as any, cwd)

  assert.ok(first)
  assert.ok(second)
  assert.notEqual(first?.configPath, second?.configPath)
  assert.equal(existsSync(first!.configPath), true)
  assert.equal(existsSync(second!.configPath), true)
  assert.equal(statSync(first!.configPath).mode & 0o777, 0o600)
  assert.deepEqual(JSON.parse(readFileSync(first!.configPath, 'utf8')), {
    mcpServers: { one: { exposure: 'direct', command: 'one', args: [], env: {}, cwd } }
  })
  assert.equal(process.env.PI_ACP_MCP_CONFIG_FILE, undefined)

  first!.cleanup()
  first!.cleanup()
  assert.equal(existsSync(first!.configPath), false)
  assert.equal(existsSync(second!.configPath), true)
  second!.cleanup()
  assert.equal(existsSync(second!.configPath), false)
})

test('MCP bridge leaves the no-server path untouched', () => {
  assert.equal(createMcpBridge([], cwd), null)
  assert.deepEqual(mapMcpServers([], cwd), {})
})
