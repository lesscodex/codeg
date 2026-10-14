"use client"

import { useCallback, useEffect, useRef, useState } from "react"
import type { PromptDraft } from "@/lib/types"
import {
  activateScheduledMessageRuntime,
  createScheduledMessage,
  getScheduledMessage,
  listScheduledMessages,
  markScheduledMessageFailed,
  markScheduledMessageWaiting,
  prepareScheduledMessages,
  removeScheduledMessage,
  rescheduleMessage,
  subscribeScheduledMessages,
  type ScheduledMessage,
} from "@/lib/scheduled-messages"

const MAX_TIMER_DELAY_MS = 60_000
const RECONNECT_RETRY_MS = 5_000
const BUSY_RETRY_MS = 1_000
const deliveryClaims = new Set<string>()
const acceptedDeliveries = new Set<string>()

function scheduledDeliveryLockName(id: string): string {
  return `codeg:scheduled-message-delivery:${id}`
}

interface ScheduledDeliveryCallbacks {
  accepted: () => void
  busy: () => void
  failed: () => void
}

interface UseScheduledMessagesOptions {
  scopeId: string
  connectionReady: boolean
  prompting: boolean
  deliver: (draft: PromptDraft, callbacks: ScheduledDeliveryCallbacks) => void
  ensureReady?: () => void
}

export function useScheduledMessages({
  scopeId,
  connectionReady,
  prompting,
  deliver,
  ensureReady,
}: UseScheduledMessagesOptions) {
  const [messages, setMessages] = useState<ScheduledMessage[]>(() =>
    listScheduledMessages(scopeId)
  )
  const retryAfterRef = useRef(new Map<string, number>())
  const deliverRef = useRef(deliver)
  const ensureReadyRef = useRef(ensureReady)

  useEffect(() => {
    deliverRef.current = deliver
  }, [deliver])
  useEffect(() => {
    ensureReadyRef.current = ensureReady
  }, [ensureReady])

  const refresh = useCallback(
    () => setMessages(listScheduledMessages(scopeId)),
    [scopeId]
  )

  useEffect(() => {
    activateScheduledMessageRuntime()
    prepareScheduledMessages(scopeId)
    const timer = window.setTimeout(refresh, 0)
    const unsubscribe = subscribeScheduledMessages(refresh)
    return () => {
      window.clearTimeout(timer)
      unsubscribe()
    }
  }, [refresh, scopeId])

  useEffect(() => {
    const now = Date.now()
    const due = messages.filter(
      (item) =>
        item.status !== "missed" &&
        item.status !== "failed" &&
        !acceptedDeliveries.has(item.id) &&
        item.scheduledAt <= now
    )

    if (due.length > 0) {
      const item = due[0]
      const retryAfter = retryAfterRef.current.get(item.id) ?? 0
      if (retryAfter > now) {
        const timer = window.setTimeout(refresh, retryAfter - now)
        return () => window.clearTimeout(timer)
      }
      if (prompting || !connectionReady) {
        if (item.status === "scheduled") {
          try {
            markScheduledMessageWaiting(item.id)
          } catch {
            return
          }
          return
        }
        if (!prompting && !connectionReady) {
          ensureReadyRef.current?.()
          const timer = window.setTimeout(refresh, RECONNECT_RETRY_MS)
          return () => window.clearTimeout(timer)
        }
      } else if (!deliveryClaims.has(item.id)) {
        deliveryClaims.add(item.id)
        const runDelivery = async () => {
          const current = getScheduledMessage(item.id)
          if (
            !current ||
            current.scopeId !== scopeId ||
            current.status === "missed" ||
            current.status === "failed" ||
            current.scheduledAt > Date.now()
          ) {
            return
          }
          await new Promise<void>((resolve) => {
            let settled = false
            const settle = (change: () => void) => {
              if (settled) return
              settled = true
              change()
              resolve()
            }
            const fail = () =>
              settle(() => {
                retryAfterRef.current.delete(item.id)
                try {
                  markScheduledMessageFailed(item.id)
                } catch {
                  refresh()
                }
              })
            try {
              deliverRef.current(current.draft, {
                accepted: () =>
                  settle(() => {
                    acceptedDeliveries.add(item.id)
                    retryAfterRef.current.delete(item.id)
                    try {
                      removeScheduledMessage(item.id)
                    } catch {
                      refresh()
                    }
                  }),
                busy: () =>
                  settle(() => {
                    retryAfterRef.current.set(
                      item.id,
                      Date.now() + BUSY_RETRY_MS
                    )
                    try {
                      markScheduledMessageWaiting(item.id)
                    } catch {
                      refresh()
                    }
                  }),
                failed: fail,
              })
            } catch {
              fail()
            }
          })
        }

        const locks =
          typeof navigator === "undefined" ? undefined : navigator.locks
        if (!locks) {
          void runDelivery().finally(() => deliveryClaims.delete(item.id))
        } else {
          void locks
            .request(
              scheduledDeliveryLockName(item.id),
              { ifAvailable: true },
              async (lock) => {
                if (!lock) {
                  window.setTimeout(refresh, BUSY_RETRY_MS)
                  return
                }
                await runDelivery()
              }
            )
            .catch(runDelivery)
            .finally(() => deliveryClaims.delete(item.id))
        }
      }
      return
    }

    const next = messages.find(
      (item) =>
        item.status !== "missed" &&
        item.status !== "failed" &&
        !acceptedDeliveries.has(item.id) &&
        item.scheduledAt > now
    )
    if (!next) return
    const timer = window.setTimeout(
      refresh,
      Math.min(next.scheduledAt - now, MAX_TIMER_DELAY_MS)
    )
    return () => window.clearTimeout(timer)
  }, [connectionReady, messages, prompting, refresh, scopeId])

  const schedule = useCallback(
    (draft: PromptDraft, scheduledAt: number) =>
      createScheduledMessage(scopeId, draft, scheduledAt),
    [scopeId]
  )

  const reschedule = useCallback((id: string, scheduledAt: number) => {
    acceptedDeliveries.delete(id)
    rescheduleMessage(id, scheduledAt)
  }, [])

  const remove = useCallback((id: string) => {
    acceptedDeliveries.delete(id)
    removeScheduledMessage(id)
  }, [])

  return {
    messages,
    schedule,
    reschedule,
    remove,
  }
}
