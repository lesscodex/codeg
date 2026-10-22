import { readFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { createMcpExtension, type ExtensionAPI, type McpServerConfig } from '@earendil-works/pi-coding-agent'

const MCP_CONFIG_ENV = 'PI_ACP_MCP_CONFIG_FILE'

function readServers(configPath: string): Record<string, McpServerConfig> {
  let value: unknown
  try {
    value = JSON.parse(readFileSync(configPath, 'utf8'))
  } catch (error) {
    throw new Error(`pi-acp MCP bridge configuration could not be read: ${String(error)}`)
  }

  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw new Error('pi-acp MCP bridge configuration must be an object')
  }
  const servers = (value as { mcpServers?: unknown }).mcpServers
  if (typeof servers !== 'object' || servers === null || Array.isArray(servers)) {
    throw new Error('pi-acp MCP bridge configuration must contain an mcpServers object')
  }

  return servers as Record<string, McpServerConfig>
}

/** Pi extension entrypoint used only for one ACP subprocess. */
export default function registerAcpMcpServers(pi: ExtensionAPI): void {
  const configPath = process.env[MCP_CONFIG_ENV]
  if (!configPath) return

  const servers = readServers(configPath)
  const registered: string[] = []
  for (const [name, config] of Object.entries(servers)) {
    if (!config || typeof config !== 'object' || Array.isArray(config)) {
      throw new Error(`pi-acp MCP bridge entry "${name}" must be an object`)
    }
    if (name === 'codeg-mcp') {
      // Runtime registrations in third-party MCP adapters lose direct exposure.
      // Keep the companion on Pi's own client without sharing their registry.
      createMcpExtension({
        loadConfig: () => ({ servers: [{ name, config, source: configPath }], errors: [] }),
        logPath: join(dirname(configPath), 'codeg-mcp.log')
      })({ ...pi, getMcpServers: () => [], registerCommand: () => {} })
      continue
    }
    pi.registerMcpServer(name, config)
    registered.push(name)
  }

  pi.on('session_shutdown', () => {
    for (const name of registered) pi.unregisterMcpServer(name)
  })
}
