"use client"

import { useEffect, useState } from "react"
import {
  listScheduledMessages,
  SCHEDULED_MESSAGE_CHANGED_EVENT,
} from "@/lib/api"
import { onTransportReconnect, subscribe } from "@/lib/platform"
import type { ScheduledMessage } from "@/lib/types"

const EMPTY: ScheduledMessage[] = []

export function useScheduledMessages(conversationId: number | null) {
  const [loaded, setLoaded] = useState<{
    conversationId: number
    messages: ScheduledMessage[]
  } | null>(null)

  useEffect(() => {
    if (conversationId == null) return
    let active = true
    let sequence = 0
    let unsubscribe: (() => void) | undefined
    const refresh = () => {
      const request = ++sequence
      void listScheduledMessages(conversationId)
        .then((messages) => {
          if (active && request === sequence) {
            setLoaded({ conversationId, messages })
          }
        })
        .catch((error) => {
          if (active && request === sequence) {
            console.warn(
              "[scheduled-message] could not load scheduled messages",
              error
            )
          }
        })
    }
    const offReconnect = onTransportReconnect(refresh)
    void subscribe(SCHEDULED_MESSAGE_CHANGED_EVENT, refresh)
      .then((off) => {
        if (active) {
          unsubscribe = off
          refresh()
        } else {
          off()
        }
      })
      .catch(() => {
        if (active) refresh()
      })
    return () => {
      active = false
      unsubscribe?.()
      offReconnect?.()
    }
  }, [conversationId])

  return loaded?.conversationId === conversationId ? loaded.messages : EMPTY
}
