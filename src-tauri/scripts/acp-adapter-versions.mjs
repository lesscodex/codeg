#!/usr/bin/env node

import * as fs from "node:fs"
import { dirname, join, resolve } from "node:path"
import { fileURLToPath } from "node:url"

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url))
const REPO_ROOT = resolve(SCRIPT_DIR, "../..")
const REGISTRY_RELATIVE = "src-tauri/src/acp/registry.rs"

function fail(message) {
  throw new Error(message)
}

function readRegistry(fsApi, repoRoot) {
  const file = join(repoRoot, REGISTRY_RELATIVE)
  try {
    return fsApi.readFileSync(file, "utf8")
  } catch (error) {
    fail(`cannot read ACP registry ${file}: ${error.message}`)
  }
}

function rustStringConstant(source, name) {
  const match = source.match(new RegExp(`pub const ${name}: &str = "([^"]+)";`))
  if (!match) fail(`ACP registry is missing ${name}`)
  return match[1]
}

function distributionVersion(source, agent, distribution) {
  const marker = `AgentType::${agent} => AcpAgentMeta {`
  const start = source.indexOf(marker)
  if (start < 0) fail(`ACP registry is missing the ${agent} entry`)
  const end = source.indexOf("\n        AgentType::", start + marker.length)
  const block = source.slice(start, end < 0 ? source.length : end)
  const distributionMarker = `distribution: AgentDistribution::${distribution} {`
  const distributionStart = block.indexOf(distributionMarker)
  if (distributionStart < 0) {
    fail(`${agent} does not use the expected ${distribution} distribution`)
  }
  const match = block
    .slice(distributionStart + distributionMarker.length)
    .match(/^\s*version:\s*"([^"]+)",/m)
  if (!match) fail(`${agent} ${distribution} distribution has no version`)
  return match[1]
}

export function acpVersion(key, { fsApi = fs, repoRoot = REPO_ROOT } = {}) {
  const source = readRegistry(fsApi, resolve(repoRoot))
  switch (key) {
    case "codex-adapter":
      return distributionVersion(source, "Codex", "Npx")
    case "codex-runtime":
      return rustStringConstant(source, "CODEX_NESTED_VERSION_FALLBACK")
    case "pi-adapter":
      return rustStringConstant(source, "PI_ACP_ADAPTER_VERSION")
    case "pi-upstream":
      return distributionVersion(source, "Pi", "Npx")
    case "pi-runtime":
      return rustStringConstant(source, "PI_MIN_RUNTIME_VERSION")
    default:
      fail(`unknown ACP version key: ${key}`)
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const key = process.argv[2]
  if (!key || process.argv.length !== 3) {
    fail("usage: acp-adapter-versions.mjs <version-key>")
  }
  process.stdout.write(`${acpVersion(key)}\n`)
}
