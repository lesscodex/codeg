import { beforeEach, describe, expect, it, vi } from "vitest"
import type { PromptDraft } from "@/lib/types"
import {
  SCHEDULED_MESSAGE_STORAGE_PREFIX,
  SCHEDULED_MESSAGE_RUNTIME_PREFIX,
  createScheduledMessage,
  listScheduledMessages,
  prepareScheduledMessages,
  removeScheduledMessage,
  rescheduleMessage,
  scheduledMessageRuntimeId,
} from "@/lib/scheduled-messages"

const draft = (text: string): PromptDraft => ({
  blocks: [{ type: "text", text }],
  displayText: text,
})

describe("scheduled messages storage", () => {
  beforeEach(() => {
    window.localStorage.clear()
  })

  it("persists, reschedules and removes messages by scope", () => {
    const first = createScheduledMessage("tab-a", draft("later"), 2_000, 1_000)
    createScheduledMessage("tab-b", draft("other"), 3_000, 1_000)

    expect(listScheduledMessages("tab-a")).toEqual([first])
    rescheduleMessage(first.id, 4_000, 1_000)
    expect(listScheduledMessages("tab-a")[0]).toMatchObject({
      scheduledAt: 4_000,
      status: "scheduled",
    })
    removeScheduledMessage(first.id)
    expect(listScheduledMessages("tab-a")).toEqual([])

    const other = listScheduledMessages("tab-b")[0]
    removeScheduledMessage(other.id)
    expect(listScheduledMessages("tab-b")).toEqual([])
  })

  it("marks a previous runtime's overdue messages missed and adopts future ones", () => {
    const stored = [
      {
        id: "late",
        scopeId: "tab-a",
        draft: draft("late"),
        scheduledAt: 900,
        status: "scheduled",
        runtimeId: "old-runtime",
        createdAt: 100,
      },
      {
        id: "future",
        scopeId: "tab-a",
        draft: draft("future"),
        scheduledAt: 2_000,
        status: "waiting",
        runtimeId: "old-runtime",
        createdAt: 100,
      },
      {
        id: "failed",
        scopeId: "tab-a",
        draft: draft("failed"),
        scheduledAt: 800,
        status: "failed",
        runtimeId: "old-runtime",
        createdAt: 100,
      },
    ]
    for (const item of stored) {
      window.localStorage.setItem(
        `${SCHEDULED_MESSAGE_STORAGE_PREFIX}${item.id}`,
        JSON.stringify(item)
      )
    }

    prepareScheduledMessages("tab-a", 1_000)

    expect(listScheduledMessages("tab-a")).toEqual([
      expect.objectContaining({ id: "failed", status: "failed" }),
      expect.objectContaining({ id: "late", status: "missed" }),
      expect.objectContaining({
        id: "future",
        status: "scheduled",
        runtimeId: scheduledMessageRuntimeId(),
      }),
    ])
  })

  it("keeps the draft when localStorage rejects the write", () => {
    const setItem = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementationOnce(() => {
        throw new DOMException("quota", "QuotaExceededError")
      })

    expect(() =>
      createScheduledMessage("tab-a", draft("large"), 2_000, 1_000)
    ).toThrow("quota")
    expect(listScheduledMessages("tab-a")).toEqual([])
    setItem.mockRestore()
  })

  it("does not steal or miss a message owned by another active page", () => {
    const item = {
      id: "owned",
      scopeId: "tab-a",
      draft: draft("owned"),
      scheduledAt: 900,
      status: "scheduled",
      runtimeId: "other-runtime",
      createdAt: 100,
    }
    window.localStorage.setItem(
      `${SCHEDULED_MESSAGE_STORAGE_PREFIX}${item.id}`,
      JSON.stringify(item)
    )
    window.localStorage.setItem(
      `${SCHEDULED_MESSAGE_RUNTIME_PREFIX}${item.runtimeId}`,
      "1000"
    )

    prepareScheduledMessages("tab-a", 1_000)

    expect(listScheduledMessages("tab-a")).toEqual([item])
  })
})
