import test from 'node:test'
import assert from 'node:assert/strict'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { PiAcpAgent } from '../../src/acp/agent.js'
import { PiRpcProcess } from '../../src/pi-rpc/process.js'
import { FakeAgentSideConnection, asAgentConn } from '../helpers/fakes.js'

test('PiAcpAgent passes supplied MCP servers to both new and restored sessions', async () => {
  const home = mkdtempSync(join(tmpdir(), 'pi-acp-session-wiring-'))
  const previousHome = process.env.HOME
  process.env.HOME = home
  const spawnCalls: any[] = []
  const sessionFile = join(home, 'session.jsonl')
  const mcpServers = [
    { name: 'tools', command: 'node', args: ['server.js'], env: [{ name: 'TOKEN', value: 'test' }] },
    { type: 'http', name: 'remote', url: 'https://example.test/mcp', headers: [{ name: 'X-Test', value: 'yes' }] }
  ]
  const originalSpawn = PiRpcProcess.spawn

  ;(PiRpcProcess as any).spawn = async (params: any) => {
    spawnCalls.push(params)
    return {
      onEvent: () => () => {},
      dispose() {},
      getState: async () => ({
        sessionId: 'session-new',
        sessionFile,
        thinkingLevel: 'medium',
        model: { provider: 'test', id: 'model' }
      }),
      getAvailableModels: async () => ({ models: [{ provider: 'test', id: 'model', name: 'Model' }] }),
      getAvailableThinkingLevels: async () => ['medium'],
      getSessionStats: async () => ({ contextUsage: { tokens: 1, contextWindow: 100 } }),
      getMessages: async () => ({ messages: [] })
    } as any
  }

  try {
    const agent = new PiAcpAgent(asAgentConn(new FakeAgentSideConnection()))
    const initialization = await agent.initialize({ protocolVersion: 1 } as any)
    assert.deepEqual(initialization.agentCapabilities?.mcpCapabilities, { http: true, sse: false })
    const created = await agent.newSession({ cwd: home, mcpServers } as any)
    await agent.loadSession({ sessionId: created.sessionId, cwd: home, mcpServers } as any)

    assert.deepEqual(spawnCalls[0].mcpServers, mcpServers)
    assert.equal(spawnCalls[0].sessionPath, undefined)
    assert.deepEqual(spawnCalls[1].mcpServers, mcpServers)
    assert.equal(spawnCalls[1].sessionPath, sessionFile)
  } finally {
    PiRpcProcess.spawn = originalSpawn
    if (previousHome === undefined) delete process.env.HOME
    else process.env.HOME = previousHome
    rmSync(home, { recursive: true, force: true })
  }
})
