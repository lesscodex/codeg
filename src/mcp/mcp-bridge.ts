import type { McpServer } from '@agentclientprotocol/sdk'
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

export const MCP_CONFIG_ENV = 'PI_ACP_MCP_CONFIG_FILE'

type PiMcpServerConfig = {
  exposure: 'direct'
  command?: string
  args?: string[]
  env?: Record<string, string>
  cwd?: string
  url?: string
  headers?: Record<string, string>
}

export class McpBridgeError extends Error {
  readonly code: 'invalid' | 'unsupported'

  constructor(message: string, code: 'invalid' | 'unsupported' = 'invalid') {
    super(message)
    this.name = 'McpBridgeError'
    this.code = code
  }
}

export type McpBridgeArtifact = {
  configPath: string
  cleanup: () => void
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null
}

function requireName(server: Record<string, unknown>): string {
  const name = server.name
  if (typeof name !== 'string' || !name) {
    throw new McpBridgeError('MCP server name must be a non-empty string')
  }
  if (!/^[A-Za-z0-9_-]+$/.test(name)) {
    throw new McpBridgeError(`MCP server "${name}" has an invalid name; use only letters, digits, "_" and "-"`)
  }
  return name
}

function mapEnvironment(serverName: string, value: unknown): Record<string, string> | undefined {
  if (value === undefined) return undefined
  if (!Array.isArray(value)) {
    throw new McpBridgeError(`MCP server "${serverName}" env must be an array`)
  }

  const env: Record<string, string> = {}
  const seen = new Set<string>()
  for (const item of value) {
    const entry = asRecord(item)
    if (!entry || typeof entry.name !== 'string' || typeof entry.value !== 'string' || !entry.name) {
      throw new McpBridgeError(`MCP server "${serverName}" env entries need name and value strings`)
    }
    const key = entry.name.toLowerCase()
    if (seen.has(key)) {
      throw new McpBridgeError(`MCP server "${serverName}" has duplicate environment variable "${entry.name}"`)
    }
    seen.add(key)
    env[entry.name] = entry.value
  }
  return env
}

function mapHeaders(serverName: string, value: unknown): Record<string, string> | undefined {
  if (value === undefined) return undefined
  if (!Array.isArray(value)) {
    throw new McpBridgeError(`MCP server "${serverName}" headers must be an array`)
  }

  const headers: Record<string, string> = {}
  const seen = new Set<string>()
  for (const item of value) {
    const entry = asRecord(item)
    if (!entry || typeof entry.name !== 'string' || typeof entry.value !== 'string' || !entry.name) {
      throw new McpBridgeError(`MCP server "${serverName}" headers need name and value strings`)
    }
    const key = entry.name.toLowerCase()
    if (seen.has(key)) {
      throw new McpBridgeError(`MCP server "${serverName}" has duplicate HTTP header "${entry.name}"`)
    }
    seen.add(key)
    headers[entry.name] = entry.value
  }
  return headers
}

function mapArgs(serverName: string, value: unknown): string[] {
  if (value === undefined) return []
  if (!Array.isArray(value) || value.some(arg => typeof arg !== 'string')) {
    throw new McpBridgeError(`MCP server "${serverName}" args must be an array of strings`)
  }
  return value as string[]
}

function mapHttpServer(serverName: string, server: Record<string, unknown>): PiMcpServerConfig {
  if (typeof server.url !== 'string' || !server.url) {
    throw new McpBridgeError(`MCP server "${serverName}" HTTP url must be a non-empty string`)
  }

  let url: URL
  try {
    url = new URL(server.url)
  } catch {
    throw new McpBridgeError(`MCP server "${serverName}" HTTP url is invalid`)
  }
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    throw new McpBridgeError(`MCP server "${serverName}" HTTP url must use http or https`)
  }

  const headers = mapHeaders(serverName, server.headers)
  return {
    exposure: 'direct',
    url: server.url,
    ...(server.headers !== undefined ? { headers: headers ?? {} } : {})
  }
}

function mapStdioServer(serverName: string, server: Record<string, unknown>, cwd: string): PiMcpServerConfig {
  if (typeof server.command !== 'string' || !server.command.trim()) {
    throw new McpBridgeError(`MCP server "${serverName}" command must be a non-empty string`)
  }

  const args = mapArgs(serverName, server.args)
  const env = mapEnvironment(serverName, server.env)
  return {
    exposure: 'direct',
    command: server.command,
    ...(server.args !== undefined ? { args } : {}),
    ...(server.env !== undefined ? { env: env ?? {} } : {}),
    cwd
  }
}

/**
 * Convert ACP's list-shaped MCP transport descriptions to Pi's native mcpServers map.
 * The returned object is safe to serialize into a per-process bridge file.
 */
export function mapMcpServers(mcpServers: readonly McpServer[], cwd: string): Record<string, PiMcpServerConfig> {
  if (!Array.isArray(mcpServers)) throw new McpBridgeError('MCP servers must be an array')

  const mapped: Record<string, PiMcpServerConfig> = {}
  const normalizedNames = new Set<string>()

  for (const rawServer of mcpServers) {
    const server = asRecord(rawServer)
    if (!server) throw new McpBridgeError('MCP server entries must be objects')

    const name = requireName(server)
    const normalizedName = name.replace(/-/g, '_')
    if (normalizedNames.has(normalizedName)) {
      throw new McpBridgeError(`MCP server name collision for "${name}"`)
    }
    normalizedNames.add(normalizedName)

    const type = server.type
    if (type === 'sse') {
      throw new McpBridgeError(
        `MCP server "${name}" uses legacy SSE, which Pi does not support; provide a streamable HTTP URL instead`,
        'unsupported'
      )
    }
    if (type === 'acp') {
      throw new McpBridgeError(`MCP server "${name}" uses ACP transport, which Pi cannot host`, 'unsupported')
    }
    if (type !== undefined && type !== 'stdio' && type !== 'http' && type !== 'streamable-http') {
      throw new McpBridgeError(`MCP server "${name}" uses unsupported transport "${String(type)}"`, 'unsupported')
    }

    const hasUrl = typeof server.url === 'string'
    const hasCommand = typeof server.command === 'string'
    if (hasUrl && hasCommand) {
      throw new McpBridgeError(`MCP server "${name}" must specify either command or url, not both`)
    }

    if (type === 'stdio') {
      if (!hasCommand) throw new McpBridgeError(`MCP server "${name}" stdio transport needs a command`)
      mapped[name] = mapStdioServer(name, server, cwd)
      continue
    }
    if (type === 'http' || type === 'streamable-http') {
      if (!hasUrl) throw new McpBridgeError(`MCP server "${name}" HTTP transport needs a url`)
      mapped[name] = mapHttpServer(name, server)
      continue
    }
    if (hasUrl) {
      mapped[name] = mapHttpServer(name, server)
      continue
    }
    if (hasCommand) {
      mapped[name] = mapStdioServer(name, server, cwd)
      continue
    }
    throw new McpBridgeError(`MCP server "${name}" needs a command or streamable HTTP url`)
  }

  return mapped
}

/** Validate transport/name fields without writing any session-specific data. */
export function validateMcpServers(mcpServers: readonly McpServer[], cwd: string): void {
  mapMcpServers(mcpServers, cwd)
}

/** Create an ephemeral bridge file. The caller owns cleanup and must invoke it on every exit path. */
export function createMcpBridge(mcpServers: readonly McpServer[], cwd: string): McpBridgeArtifact | null {
  const servers = mapMcpServers(mcpServers, cwd)
  if (Object.keys(servers).length === 0) return null

  const directory = mkdtempSync(join(tmpdir(), 'pi-acp-mcp-'))
  const configPath = join(directory, 'servers.json')
  try {
    writeFileSync(configPath, `${JSON.stringify({ mcpServers: servers })}\n`, {
      encoding: 'utf8',
      mode: 0o600
    })
  } catch (error) {
    rmSync(directory, { recursive: true, force: true })
    throw new McpBridgeError(`Could not create the temporary MCP bridge configuration: ${String(error)}`)
  }

  let cleaned = false
  return {
    configPath,
    cleanup: () => {
      if (cleaned) return
      cleaned = true
      rmSync(directory, { recursive: true, force: true })
    }
  }
}
