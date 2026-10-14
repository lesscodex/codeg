"use client"

import { useEffect } from "react"
import {
  importLegacyScheduledMessage,
  type LegacyScheduledMessage,
} from "@/lib/api"
import { isRemoteDesktopWindow, onTransportReady } from "@/lib/platform"

const MESSAGE_PREFIX = "codeg:scheduled-message:v1:"
const RUNTIME_PREFIX = "codeg:scheduled-runtime:v1:"

function legacyKeys(storage: Storage, prefix: string): string[] {
  const keys: string[] = []
  for (let index = 0; index < storage.length; index++) {
    const key = storage.key(index)
    if (key?.startsWith(prefix)) keys.push(key)
  }
  return keys
}

function parseRecord(raw: string | null): Record<string, unknown> | null {
  try {
    const value: unknown = JSON.parse(raw ?? "null")
    return value != null && typeof value === "object"
      ? (value as Record<string, unknown>)
      : null
  } catch {
    return null
  }
}

function parseMessage(key: string, raw: string): LegacyScheduledMessage | null {
  const record = parseRecord(raw)
  if (!record) return null
  const { id, scopeId, draft, scheduledAt, status, runtimeId, createdAt } =
    record
  if (
    typeof id !== "string" ||
    !id ||
    key !== MESSAGE_PREFIX + id ||
    typeof scopeId !== "string" ||
    !/^conversation:[1-9]\d*$/.test(scopeId) ||
    draft == null ||
    typeof scheduledAt !== "number" ||
    !Number.isSafeInteger(scheduledAt) ||
    typeof status !== "string" ||
    !["scheduled", "waiting", "missed", "failed"].includes(status) ||
    typeof runtimeId !== "string" ||
    typeof createdAt !== "number" ||
    !Number.isFinite(createdAt)
  ) {
    return null
  }
  return { id, scopeId, draft, scheduledAt, status }
}

function removeUnusedHeartbeats(storage: Storage) {
  const runtimes = new Set<string>()
  for (const key of legacyKeys(storage, MESSAGE_PREFIX)) {
    const raw = storage.getItem(key)
    if (raw == null) continue
    const runtimeId = parseRecord(raw)?.runtimeId
    if (typeof runtimeId !== "string") return
    runtimes.add(RUNTIME_PREFIX + runtimeId)
  }
  for (const key of legacyKeys(storage, RUNTIME_PREFIX)) {
    if (!runtimes.has(key)) storage.removeItem(key)
  }
}

async function migrate(): Promise<boolean> {
  const storage = window.localStorage
  let retry = false
  for (const key of legacyKeys(storage, MESSAGE_PREFIX)) {
    const raw = storage.getItem(key)
    if (raw == null) continue
    const message = parseMessage(key, raw)
    if (!message) continue

    const importOne = async () => {
      if (storage.getItem(key) !== raw) return
      const confirmed = await importLegacyScheduledMessage(message)
      if (confirmed === true && storage.getItem(key) === raw) {
        storage.removeItem(key)
      }
    }
    try {
      // Coordinate with a v1 page that can still be open during upgrade.
      if (navigator.locks) {
        await navigator.locks.request(
          `codeg:scheduled-message-delivery:${message.id}`,
          { ifAvailable: true },
          async (lock) => {
            if (lock) await importOne()
            else retry = true
          }
        )
      } else {
        await importOne()
      }
    } catch {
      retry = true
    }
  }
  removeUnusedHeartbeats(storage)
  return retry
}

export function ScheduledMessageMigration() {
  useEffect(() => {
    if (isRemoteDesktopWindow()) return
    let disposed = false
    let running = false
    let timer: number | undefined
    let delay = 1000

    const run = async () => {
      if (disposed || running) return
      running = true
      try {
        const retry = await migrate()
        if (retry && !disposed) {
          timer = window.setTimeout(run, delay)
          delay = Math.min(delay * 2, 30_000)
        }
      } catch {
        // Browser storage can be unavailable. Leave any source data intact.
      } finally {
        running = false
      }
    }

    const offReady = onTransportReady(() => {
      window.clearTimeout(timer)
      delay = 1000
      void run()
    })
    void run()
    return () => {
      disposed = true
      window.clearTimeout(timer)
      offReady?.()
    }
  }, [])
  return null
}
