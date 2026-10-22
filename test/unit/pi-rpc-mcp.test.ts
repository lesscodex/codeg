import test from 'node:test'
import assert from 'node:assert/strict'
import { chmodSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { PiRpcProcess } from '../../src/pi-rpc/process.js'

function writeFakePi(root: string): string {
  const launcher = join(root, 'fake-pi')
  writeFileSync(
    launcher,
    [
      '#!/usr/bin/env node',
      "const { appendFileSync, readFileSync } = require('node:fs')",
      "const readline = require('node:readline')",
      'const args = process.argv.slice(2)',
      "const sessionPath = args[args.indexOf('--session') + 1]",
      "const marker = sessionPath + '.launch.json'",
      "appendFileSync(marker, JSON.stringify({ args, configPath: process.env.PI_ACP_MCP_CONFIG_FILE }) + '\\n')",
      "readline.createInterface({ input: process.stdin }).on('line', line => {",
      '  const command = JSON.parse(line)',
      "  process.stdout.write(JSON.stringify({ type: 'response', id: command.id, command: command.type, success: true, data: { sessionFile: sessionPath } }) + '\\n')",
      '})',
      'process.stdin.resume()',
      'setInterval(() => {}, 1000)'
    ].join('\n')
  )
  chmodSync(launcher, 0o755)
  return launcher
}

test('PiRpcProcess scopes MCP bridge files to each subprocess and cleans them on dispose', async () => {
  const root = mkdtempSync(join(tmpdir(), 'pi-acp-mcp-process-'))
  const launcher = writeFakePi(root)
  const firstSession = join(root, 'first.jsonl')
  const secondSession = join(root, 'second.jsonl')
  mkdirSync(root, { recursive: true })

  try {
    const [first, second] = await Promise.all([
      PiRpcProcess.spawn({
        cwd: root,
        piCommand: launcher,
        sessionPath: firstSession,
        mcpServers: [{ name: 'one', command: 'one', args: [], env: [] } as any]
      }),
      PiRpcProcess.spawn({
        cwd: root,
        piCommand: launcher,
        sessionPath: secondSession,
        mcpServers: [{ name: 'two', command: 'two', args: [], env: [] } as any]
      })
    ])

    const firstLaunch = JSON.parse(readFileSync(`${firstSession}.launch.json`, 'utf8'))
    const secondLaunch = JSON.parse(readFileSync(`${secondSession}.launch.json`, 'utf8'))
    assert.notEqual(firstLaunch.configPath, secondLaunch.configPath)
    assert.equal(firstLaunch.args.includes('--extension'), true)
    assert.equal(secondLaunch.args.includes('--extension'), true)
    assert.equal(existsSync(firstLaunch.configPath), true)
    assert.equal(existsSync(secondLaunch.configPath), true)

    first.dispose()
    second.dispose()
    await new Promise(resolve => setTimeout(resolve, 50))
    assert.equal(existsSync(firstLaunch.configPath), false)
    assert.equal(existsSync(secondLaunch.configPath), false)
  } finally {
    rmSync(root, { recursive: true, force: true })
  }
})

test('PiRpcProcess does not inject a bridge for the no-server path', async () => {
  const root = mkdtempSync(join(tmpdir(), 'pi-acp-no-mcp-process-'))
  const launcher = writeFakePi(root)
  const sessionPath = join(root, 'session.jsonl')
  const previous = process.env.PI_ACP_MCP_CONFIG_FILE
  process.env.PI_ACP_MCP_CONFIG_FILE = 'must-not-leak'

  try {
    const proc = await PiRpcProcess.spawn({ cwd: root, piCommand: launcher, sessionPath })
    const launch = JSON.parse(readFileSync(`${sessionPath}.launch.json`, 'utf8'))
    assert.equal(launch.args.includes('--extension'), false)
    assert.equal(launch.configPath, undefined)
    proc.dispose()
  } finally {
    if (previous === undefined) delete process.env.PI_ACP_MCP_CONFIG_FILE
    else process.env.PI_ACP_MCP_CONFIG_FILE = previous
    rmSync(root, { recursive: true, force: true })
  }
})
