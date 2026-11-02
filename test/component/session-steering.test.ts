import test from 'node:test'
import assert from 'node:assert/strict'
import { PiAcpSession } from '../../src/acp/session.js'
import { FakeAgentSideConnection, FakePiRpcProcess, asAgentConn } from '../helpers/fakes.js'

function makeSession(proc = new FakePiRpcProcess()) {
  const conn = new FakeAgentSideConnection()
  const session = new PiAcpSession({
    sessionId: 's1',
    cwd: process.cwd(),
    mcpServers: [],
    proc: proc as any,
    conn: asAgentConn(conn),
    fileCommands: []
  })
  return { conn, proc, session }
}

test('PiAcpSession injects steering into the active prompt and keeps its owner open', async () => {
  const { proc, session } = makeSession()
  const prompt = session.prompt('initial')

  assert.deepEqual(await session.steer('adjust', [{ type: 'image', data: 'abc' }]), 'injected')
  assert.deepEqual(await session.steer('second adjustment'), 'injected')
  assert.deepEqual(proc.steers, [
    { message: 'adjust', attachments: [{ type: 'image', data: 'abc' }] },
    { message: 'second adjustment', attachments: [] }
  ])

  proc.emit({ type: 'agent_start' })
  proc.emit({ type: 'turn_end' })
  proc.emit({ type: 'agent_end' })
  await Promise.resolve()
  let promptSettled = false
  void prompt.then(() => {
    promptSettled = true
  })
  await Promise.resolve()
  assert.equal(promptSettled, false)

  proc.emit({ type: 'agent_settled' })
  assert.equal(await prompt, 'end_turn')
})

test('PiAcpSession rejects steering after agent_settled without consuming it', async () => {
  const { proc, session } = makeSession()
  const prompt = session.prompt('initial')

  proc.emit({ type: 'agent_start' })
  proc.emit({ type: 'agent_settled' })

  assert.deepEqual(await session.steer('too late'), 'promptRequired')
  assert.deepEqual(proc.steers, [])
  assert.equal(await prompt, 'end_turn')
})

test('PiAcpSession maps a rejected Pi steer to promptRequired and preserves prompt ownership', async () => {
  const proc = new FakePiRpcProcess()
  proc.steerError = new Error('pi steer failed: not streaming')
  const { session } = makeSession(proc)
  const prompt = session.prompt('initial')

  assert.deepEqual(await session.steer('retry me'), 'promptRequired')

  proc.emit({ type: 'agent_start' })
  proc.emit({ type: 'agent_settled' })
  assert.equal(await prompt, 'end_turn')
})

test('PiAcpSession resolves queued prompts when the owning Pi prompt is rejected', async () => {
  const proc = new FakePiRpcProcess()
  proc.prompt = async () => {
    throw new Error('pi prompt failed: source rejected')
  }
  const { session } = makeSession(proc)

  const first = session.prompt('initial')
  const queued = session.prompt('queued')

  assert.equal(await first, 'error')
  assert.equal(await queued, 'error')
})

test('PiAcpSession cancellation clears adapter and Pi-native queues', async () => {
  const { proc, session } = makeSession()
  const first = session.prompt('initial')
  const queued = session.prompt('queued')

  await session.cancel()

  assert.equal(proc.clearQueueCount, 1)
  assert.equal(proc.abortCount, 1)
  assert.equal(await queued, 'cancelled')

  proc.emit({ type: 'agent_settled' })
  assert.equal(await first, 'cancelled')
})

test('PiAcpSession does not abort when Pi-native queue cleanup fails', async () => {
  const proc = new FakePiRpcProcess()
  proc.clearQueueError = new Error('pi clear_queue failed')
  const { session } = makeSession(proc)
  const first = session.prompt('initial')

  await assert.rejects(session.cancel(), /pi clear_queue failed/)
  assert.equal(proc.clearQueueCount, 1)
  assert.equal(proc.abortCount, 0)

  proc.emit({ type: 'agent_settled' })
  assert.equal(await first, 'cancelled')
})
