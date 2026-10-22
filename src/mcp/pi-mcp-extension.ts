import { readFileSync } from 'node:fs'

const MCP_CONFIG_ENV = 'PI_ACP_MCP_CONFIG_FILE'

type PiMcpConfig = Record<string, unknown>

type PiExtensionApi = {
  registerMcpServer: (name: string, config: PiMcpConfig) => void
  unregisterMcpServer?: (name: string) => void
  on?: (event: 'session_shutdown', handler: () => void) => unknown
}

function readServers(configPath: string): Record<string, PiMcpConfig> {
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

  return servers as Record<string, PiMcpConfig>
}

/** Pi extension entrypoint used only for one ACP subprocess. */
export default function registerAcpMcpServers(pi: PiExtensionApi): void {
  const configPath = process.env[MCP_CONFIG_ENV]
  if (!configPath) return

  const servers = readServers(configPath)
  const registered: string[] = []
  for (const [name, config] of Object.entries(servers)) {
    if (!config || typeof config !== 'object' || Array.isArray(config)) {
      throw new Error(`pi-acp MCP bridge entry "${name}" must be an object`)
    }
    pi.registerMcpServer(name, config)
    registered.push(name)
  }

  pi.on?.('session_shutdown', () => {
    for (const name of registered) pi.unregisterMcpServer?.(name)
  })
}
