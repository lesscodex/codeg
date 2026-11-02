import test from 'node:test'
import assert from 'node:assert/strict'
import { PiAcpAgent } from '../../src/acp/agent.js'
import { FakeAgentSideConnection, asAgentConn } from '../helpers/fakes.js'

class FakeSessions {
  constructor(private readonly session: any) {}

  maybeGet(_id: string) {
    return this.session
  }
}

test('PiAcpAgent advertises Codeg native steering at the top level', async () => {
  const agent = new PiAcpAgent(asAgentConn(new FakeAgentSideConnection()))
  const response = await agent.initialize({ protocolVersion: 1 } as any)

  assert.deepEqual((response as any)._meta?.steering, { supported: true })
  assert.match(response.agentInfo?.version ?? '', /codeg-steering/)
})

test('PiAcpAgent forwards steering content and images to the active session', async () => {
  const calls: Array<{ message: string; images: unknown[] }> = []
  const session = {
    sessionId: 's1',
    async steer(message: string, images: unknown[]) {
      calls.push({ message, images })
      return 'injected'
    }
  }
  const agent = new PiAcpAgent(asAgentConn(new FakeAgentSideConnection()))
  ;(agent as any).sessions = new FakeSessions(session)

  const response = await agent.extMethod('_session/steering', {
    sessionId: 's1',
    prompt: [
      { type: 'text', text: 'Use this context' },
      { type: 'resource_link', uri: 'file:///tmp/context.txt' },
      { type: 'image', mimeType: 'image/png', data: 'Zm9v' }
    ],
    _meta: { steering: { idleBehavior: 'promptRequired' } }
  })

  assert.deepEqual(response, { outcome: 'injected' })
  assert.deepEqual(calls, [
    {
      message: 'Use this context\n[Context] file:///tmp/context.txt',
      images: [{ type: 'image', mimeType: 'image/png', data: 'Zm9v' }]
    }
  ])
})

test('PiAcpAgent rejects unsupported steering input before calling the session', async () => {
  let called = false
  const session = {
    sessionId: 's1',
    async steer() {
      called = true
      return 'injected'
    }
  }
  const agent = new PiAcpAgent(asAgentConn(new FakeAgentSideConnection()))
  ;(agent as any).sessions = new FakeSessions(session)

  await assert.rejects(
    agent.extMethod('_session/steering', {
      sessionId: 's1',
      prompt: [{ type: 'video', uri: 'file:///tmp/video.mp4' }]
    }),
    /unsupported steering content block/
  )
  assert.equal(called, false)
})

test('PiAcpAgent returns promptRequired without starting a detached turn', async () => {
  const session = {
    sessionId: 's1',
    async steer() {
      return 'promptRequired'
    }
  }
  const agent = new PiAcpAgent(asAgentConn(new FakeAgentSideConnection()))
  ;(agent as any).sessions = new FakeSessions(session)

  const response = await agent.extMethod('_session/steering', {
    sessionId: 's1',
    prompt: [{ type: 'text', text: 'keep me queued' }]
  })

  assert.deepEqual(response, { outcome: 'promptRequired', reason: 'noRunningTurn' })
})
