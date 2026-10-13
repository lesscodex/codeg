"use client"

/**
 * Browser fallback for agent defaults and conversations created before selector
 * preferences were persisted by the Rust backend.
 *
 * Values are still shipped at connect time for migration compatibility. For an
 * existing conversation, the backend's stored target-conversation values take
 * precedence and confirmed ACP selector events keep that record current.
 */

import type { SessionConfigOptionInfo, SessionModeStateInfo } from "@/lib/types"

const STORAGE_KEY = "codeg:selector-prefs"
const CONVERSATION_STORAGE_KEY = "codeg:conversation-selector-prefs"

interface SelectorPrefs {
  modeId?: string
  configValues?: Record<string, string>
}

type AllPrefs = Record<string, SelectorPrefs>

function readAll(storageKey: string): AllPrefs {
  if (typeof window === "undefined") return {}
  try {
    const raw = localStorage.getItem(storageKey)
    return raw ? (JSON.parse(raw) as AllPrefs) : {}
  } catch {
    return {}
  }
}

function writeAll(storageKey: string, all: AllPrefs) {
  if (typeof window === "undefined") return
  try {
    localStorage.setItem(storageKey, JSON.stringify(all))
  } catch (error) {
    console.warn("[selector-prefs] localStorage write failed", error)
  }
}

/**
 * Built-in agent types that took over a custom agent, mapped to the custom
 * agent's id. The database migration that retired it
 * (`m20261015_000001_kiro_builtin_agent`) cannot reach this device-local
 * store, so until the built-in has picks of its own it starts from the custom
 * agent's: both ran the same `kiro-cli acp`, so its saved agent (the mode) is
 * one the built-in offers too.
 */
const RETIRED_CUSTOM_AGENTS = new Map([["kiro", "custom:kiro-cli"]])

function prefsOf(all: AllPrefs, agentType: string): SelectorPrefs | undefined {
  const own = all[agentType]
  if (own) return own
  const retired = RETIRED_CUSTOM_AGENTS.get(agentType)
  return retired ? all[retired] : undefined
}

function updatePrefs(
  agentType: string,
  conversationId: number | undefined,
  fn: (prefs: SelectorPrefs) => SelectorPrefs
) {
  const scoped = getScope(agentType, conversationId)
  const all = readAll(scoped.storageKey)
  const existing =
    scoped.storageKey === STORAGE_KEY
      ? prefsOf(all, agentType)
      : all[scoped.entryKey]
  // Re-project onto the current schema so legacy fields (`modesHash` /
  // `configHash` from before the backend took ownership of preference
  // application) don't survive across writes. Without this an upgrade
  // user's first save would re-persist the stale hash bytes forever.
  const normalized: SelectorPrefs = {
    modeId: existing?.modeId,
    configValues: existing?.configValues,
  }
  all[scoped.entryKey] = fn(normalized)
  writeAll(scoped.storageKey, all)
}

function getScope(agentType: string, conversationId?: number) {
  return conversationId != null && conversationId > 0
    ? {
        storageKey: CONVERSATION_STORAGE_KEY,
        entryKey: `${conversationId}:${agentType}`,
      }
    : { storageKey: STORAGE_KEY, entryKey: agentType }
}

function readPrefs(
  agentType: string,
  conversationId?: number
): SelectorPrefs | undefined {
  const scoped = getScope(agentType, conversationId)
  const all = readAll(scoped.storageKey)
  return scoped.storageKey === STORAGE_KEY
    ? prefsOf(all, agentType)
    : all[scoped.entryKey]
}

function configValuesFromOptions(
  options: SessionConfigOptionInfo[] | null
): Record<string, string> | undefined {
  if (options == null) return undefined
  return Object.fromEntries(
    options.map((option) => [
      option.id,
      option.kind.type === "boolean"
        ? String(option.kind.current_value)
        : option.kind.current_value,
    ])
  )
}

/**
 * cursor-agent's model-parameter vocabulary, as it appeared inside the
 * stringified variants its old picker used.
 *
 * The match below keys on these NAMES, not merely on a `key=value` shape,
 * because healing runs for every agent (see [`healLegacyValues`]) and some
 * agents accept arbitrary model ids: OpenCode builds its selector values as
 * `<provider>/<modelId>` straight from the user's own config, so
 * `mygw/llama[quant=q4]` is a perfectly legitimate id that a shape-only match
 * would truncate on every reconnect.
 */
const CURSOR_VARIANT_KEYS = new Set(["thinking", "context", "effort", "fast"])

/**
 * Heal a `model` preference saved before codeg opted into cursor-agent's
 * parameterized model picker.
 *
 * That older surface identified a model by a stringified variant —
 * `claude-opus-5[thinking=true,context=300k,effort=high,fast=false]` — and the
 * agent now rejects anything but the bare base id, so the saved value would
 * fail to apply on every connect (logged and skipped, i.e. the user silently
 * keeps landing on whatever model Cursor felt like). The base is the prefix
 * before the bracket, which is exactly what the new surface wants; the
 * parameters inside it are re-offered as their own selectors.
 *
 * Matched on the VALUE rather than on the agent id. Scoping this to
 * `agentType === "cursor"` would strand every CUSTOM agent that wraps the same
 * `cursor-agent … acp` binary: the capability gate that turns the new picker on
 * keys on the resolved launch recipe, not on the built-in identity, so those
 * agents get the new surface and carry the very same stale preference under a
 * user-chosen id this module cannot enumerate. Running for everyone is only
 * safe because the match demands cursor's own parameter names — see
 * [`CURSOR_VARIANT_KEYS`].
 */
function healLegacyValues(
  values: Record<string, string>
): Record<string, string> {
  const model = values.model
  if (typeof model !== "string" || !model.endsWith("]")) return values
  // `<= 0` also rejects a leading bracket, which would heal to an empty model.
  const open = model.lastIndexOf("[")
  if (open <= 0) return values
  // Every cursor id is a flat slug — `composer-2.5`, `claude-opus-5-thinking-
  // max-fast`. A namespaced id belongs to somebody else (OpenCode's values are
  // `<provider>/<modelId>`, and the model half comes from the user's own
  // config, so it may legitimately end in anything at all).
  if (model.slice(0, open).includes("/")) return values
  const isVariantSuffix = model
    .slice(open + 1, -1)
    .split(",")
    .every((param) => {
      const eq = param.indexOf("=")
      return eq > 0 && CURSOR_VARIANT_KEYS.has(param.slice(0, eq))
    })
  if (!isVariantSuffix) return values
  return { ...values, model: model.slice(0, open) }
}

// ── Read ──

/** Read saved mode id for an agent (no validation, just the raw value). */
export function getSavedModeId(
  agentType: string,
  conversationId?: number
): string | null {
  return readPrefs(agentType, conversationId)?.modeId ?? null
}

/**
 * Read saved agent defaults or conversation preferences. The returned shape
 * mirrors what the backend `acp_connect` command accepts (`preferred_mode_id` +
 * `preferred_config_values`). Null/empty fields are normalized so the call
 * site can pass the result through unchanged.
 *
 * The backend applies these on the freshly-attached session before any
 * `session_modes` / `session_config_options` event is emitted, so the
 * frontend never needs to "intercept event and overwrite, then sync back".
 */
export function getSavedPrefsForConnect(
  agentType: string,
  conversationId?: number
): {
  modeId: string | null
  configValues: Record<string, string> | null
} {
  const prefs = readPrefs(agentType, conversationId)
  if (!prefs) return { modeId: null, configValues: null }
  const configValues =
    prefs.configValues && Object.keys(prefs.configValues).length > 0
      ? healLegacyValues(prefs.configValues)
      : null
  return {
    modeId: prefs.modeId ?? null,
    configValues,
  }
}

// ── Save ──

export function saveModePreference(
  agentType: string,
  modes: SessionModeStateInfo,
  conversationId?: number
) {
  updatePrefs(agentType, conversationId, (prefs) => ({
    ...prefs,
    modeId: modes.current_mode_id,
  }))
}

export function saveConfigPreference(
  agentType: string,
  configId: string,
  valueId: string,
  conversationId?: number
) {
  updatePrefs(agentType, conversationId, (prefs) => ({
    ...prefs,
    configValues: { ...prefs.configValues, [configId]: valueId },
  }))
}

export function saveConversationSelectorPreferences(
  agentType: string,
  conversationId: number,
  modes: SessionModeStateInfo | null,
  configOptions: SessionConfigOptionInfo[] | null
) {
  if (conversationId <= 0) return
  const configValues = configValuesFromOptions(configOptions)
  updatePrefs(agentType, conversationId, (prefs) => ({
    modeId: modes?.current_mode_id ?? prefs.modeId,
    configValues: configValues ?? prefs.configValues,
  }))
}

export function initializeConversationSelectorPreferences(
  agentType: string,
  conversationId: number,
  modes: SessionModeStateInfo | null,
  configOptions: SessionConfigOptionInfo[] | null
) {
  if (conversationId <= 0 || readPrefs(agentType, conversationId)) return
  const defaults = readPrefs(agentType)
  const currentConfigValues = configValuesFromOptions(configOptions)
  updatePrefs(agentType, conversationId, () => ({
    modeId: defaults?.modeId ?? modes?.current_mode_id,
    configValues: {
      ...currentConfigValues,
      ...defaults?.configValues,
    },
  }))
}
