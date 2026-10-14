import type { PromptDraft } from "@/lib/types"
import { randomUUID } from "@/lib/utils"

export const SCHEDULED_MESSAGE_STORAGE_PREFIX = "codeg:scheduled-message:v1:"
export const SCHEDULED_MESSAGE_RUNTIME_PREFIX = "codeg:scheduled-runtime:v1:"
export const SCHEDULED_MESSAGES_CHANGED_EVENT =
  "codeg:scheduled-messages-changed"

const RUNTIME_HEARTBEAT_INTERVAL_MS = 10_000
const RUNTIME_ACTIVE_TTL_MS = 30_000

export type ScheduledMessageStatus =
  | "scheduled"
  | "waiting"
  | "missed"
  | "failed"

export interface ScheduledMessage {
  id: string
  scopeId: string
  draft: PromptDraft
  scheduledAt: number
  status: ScheduledMessageStatus
  runtimeId: string
  createdAt: number
}

let runtimeId: string | null = null
let runtimeHeartbeatStarted = false
let runtimeHeartbeatTimer: number | null = null

export function scheduledMessageRuntimeId(): string {
  runtimeId ??= randomUUID()
  return runtimeId
}

function runtimeStorageKey(id: string): string {
  return `${SCHEDULED_MESSAGE_RUNTIME_PREFIX}${id}`
}

function writeRuntimeHeartbeat() {
  try {
    window.localStorage.setItem(
      runtimeStorageKey(scheduledMessageRuntimeId()),
      String(Date.now())
    )
  } catch {
    // Message writes still surface storage failures to the composer. A
    // heartbeat failure only disables restart detection for this runtime.
  }
}

export function activateScheduledMessageRuntime() {
  if (typeof window === "undefined" || runtimeHeartbeatStarted) return
  runtimeHeartbeatStarted = true
  writeRuntimeHeartbeat()
  runtimeHeartbeatTimer = window.setInterval(
    writeRuntimeHeartbeat,
    RUNTIME_HEARTBEAT_INTERVAL_MS
  )
  window.addEventListener(
    "pagehide",
    () => {
      if (runtimeHeartbeatTimer != null) {
        window.clearInterval(runtimeHeartbeatTimer)
      }
      try {
        window.localStorage.removeItem(
          runtimeStorageKey(scheduledMessageRuntimeId())
        )
      } catch {
        // The stale heartbeat expires after RUNTIME_ACTIVE_TTL_MS.
      }
      runtimeHeartbeatStarted = false
      window.addEventListener("pageshow", activateScheduledMessageRuntime, {
        once: true,
      })
    },
    { once: true }
  )
}

function isRuntimeActive(id: string, now: number): boolean {
  try {
    const raw = window.localStorage.getItem(runtimeStorageKey(id))
    if (raw == null) return false
    const heartbeat = Number(raw)
    return (
      Number.isFinite(heartbeat) && now - heartbeat <= RUNTIME_ACTIVE_TTL_MS
    )
  } catch {
    return false
  }
}

function isDraft(value: unknown): value is PromptDraft {
  if (!value || typeof value !== "object") return false
  const draft = value as Partial<PromptDraft>
  return typeof draft.displayText === "string" && Array.isArray(draft.blocks)
}

function isScheduledMessage(value: unknown): value is ScheduledMessage {
  if (!value || typeof value !== "object") return false
  const item = value as Partial<ScheduledMessage>
  return (
    typeof item.id === "string" &&
    typeof item.scopeId === "string" &&
    isDraft(item.draft) &&
    typeof item.scheduledAt === "number" &&
    Number.isFinite(item.scheduledAt) &&
    (item.status === "scheduled" ||
      item.status === "waiting" ||
      item.status === "missed" ||
      item.status === "failed") &&
    typeof item.runtimeId === "string" &&
    typeof item.createdAt === "number" &&
    Number.isFinite(item.createdAt)
  )
}

function storageKey(id: string): string {
  return `${SCHEDULED_MESSAGE_STORAGE_PREFIX}${id}`
}

function parseStoredMessage(raw: string | null): ScheduledMessage | null {
  if (!raw) return null
  try {
    const parsed: unknown = JSON.parse(raw)
    return isScheduledMessage(parsed) ? parsed : null
  } catch {
    return null
  }
}

function readAll(): ScheduledMessage[] {
  if (typeof window === "undefined") return []
  try {
    const items: ScheduledMessage[] = []
    for (let index = 0; index < window.localStorage.length; index += 1) {
      const key = window.localStorage.key(index)
      if (!key?.startsWith(SCHEDULED_MESSAGE_STORAGE_PREFIX)) continue
      const item = parseStoredMessage(window.localStorage.getItem(key))
      if (item) items.push(item)
    }
    return items
  } catch {
    return []
  }
}

function notifyChanged() {
  window.dispatchEvent(new Event(SCHEDULED_MESSAGES_CHANGED_EVENT))
}

function writeItem(item: ScheduledMessage, notify = true) {
  if (typeof window === "undefined") {
    throw new Error("scheduled message storage is unavailable")
  }
  window.localStorage.setItem(storageKey(item.id), JSON.stringify(item))
  if (notify) notifyChanged()
}

export function getScheduledMessage(id: string): ScheduledMessage | null {
  if (typeof window === "undefined") return null
  try {
    return parseStoredMessage(window.localStorage.getItem(storageKey(id)))
  } catch {
    return null
  }
}

function updateItem(
  id: string,
  change: (item: ScheduledMessage) => ScheduledMessage
) {
  const item = getScheduledMessage(id)
  if (!item) return
  writeItem(change(item))
}

export function listScheduledMessages(scopeId: string): ScheduledMessage[] {
  return readAll()
    .filter((item) => item.scopeId === scopeId)
    .sort((a, b) => a.scheduledAt - b.scheduledAt || a.createdAt - b.createdAt)
}

export function createScheduledMessage(
  scopeId: string,
  draft: PromptDraft,
  scheduledAt: number,
  now: number = Date.now()
): ScheduledMessage {
  if (!scopeId || !Number.isFinite(scheduledAt) || scheduledAt <= now) {
    throw new Error("scheduled time must be in the future")
  }
  const item: ScheduledMessage = {
    id: randomUUID(),
    scopeId,
    draft,
    scheduledAt,
    status: "scheduled",
    runtimeId: scheduledMessageRuntimeId(),
    createdAt: now,
  }
  writeItem(item)
  return item
}

export function rescheduleMessage(
  id: string,
  scheduledAt: number,
  now: number = Date.now()
) {
  if (!Number.isFinite(scheduledAt) || scheduledAt <= now) {
    throw new Error("scheduled time must be in the future")
  }
  updateItem(id, (item) => ({
    ...item,
    scheduledAt,
    status: "scheduled",
    runtimeId: scheduledMessageRuntimeId(),
  }))
}

export function removeScheduledMessage(id: string) {
  if (typeof window === "undefined") {
    throw new Error("scheduled message storage is unavailable")
  }
  window.localStorage.removeItem(storageKey(id))
  notifyChanged()
}

export function markScheduledMessageWaiting(id: string) {
  updateItem(id, (item) =>
    item.status === "scheduled" ? { ...item, status: "waiting" } : item
  )
}

export function markScheduledMessageFailed(id: string) {
  updateItem(id, (item) => ({ ...item, status: "failed" }))
}

/**
 * Reconcile records after a new page/runtime starts. Future messages continue
 * in the new runtime; messages whose due time elapsed while no page was alive
 * become missed and are never auto-sent.
 */
export function prepareScheduledMessages(
  scopeId: string,
  now: number = Date.now()
) {
  const currentRuntimeId = scheduledMessageRuntimeId()
  let changed = false
  const items = listScheduledMessages(scopeId)
  for (const item of items) {
    if (
      item.runtimeId === currentRuntimeId ||
      item.status === "missed" ||
      item.status === "failed" ||
      isRuntimeActive(item.runtimeId, now)
    ) {
      continue
    }
    changed = true
    if (item.scheduledAt <= now) {
      writeItem({ ...item, status: "missed" }, false)
    } else {
      writeItem(
        {
          ...item,
          runtimeId: currentRuntimeId,
          status: "scheduled",
        },
        false
      )
    }
  }
  if (changed) notifyChanged()
}

export function subscribeScheduledMessages(listener: () => void): () => void {
  if (typeof window === "undefined") return () => {}
  const onStorage = (event: StorageEvent) => {
    if (event.key?.startsWith(SCHEDULED_MESSAGE_STORAGE_PREFIX)) listener()
  }
  window.addEventListener(SCHEDULED_MESSAGES_CHANGED_EVENT, listener)
  window.addEventListener("storage", onStorage)
  return () => {
    window.removeEventListener(SCHEDULED_MESSAGES_CHANGED_EVENT, listener)
    window.removeEventListener("storage", onStorage)
  }
}
