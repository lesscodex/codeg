// @vitest-environment jsdom

import { act, render, waitFor } from "@testing-library/react"
import { beforeEach, describe, expect, it, vi } from "vitest"
import { ScheduledMessageMigration } from "./scheduled-message-migration"

const { importMessage, onReady, isRemote } = vi.hoisted(() => ({
  importMessage: vi.fn(),
  onReady: vi.fn(),
  isRemote: vi.fn(),
}))

vi.mock("@/lib/api", () => ({
  importLegacyScheduledMessage: importMessage,
}))

vi.mock("@/lib/platform", () => ({
  isRemoteDesktopWindow: isRemote,
  onTransportReady: onReady,
}))

const MESSAGE_PREFIX = "codeg:scheduled-message:v1:"
const RUNTIME_PREFIX = "codeg:scheduled-runtime:v1:"

function saveMessage(id: string, overrides: Record<string, unknown> = {}) {
  const record = {
    id,
    scopeId: "conversation:42",
    draft: {
      displayText: "follow up",
      blocks: [{ type: "text", text: "follow up" }],
    },
    scheduledAt: 2_000,
    status: "scheduled",
    runtimeId: "runtime",
    createdAt: 1_000,
    ...overrides,
  }
  localStorage.setItem(MESSAGE_PREFIX + id, JSON.stringify(record))
  localStorage.setItem(RUNTIME_PREFIX + record.runtimeId, "1000")
  return record
}

describe("legacy scheduled-message migration", () => {
  beforeEach(() => {
    localStorage.clear()
    importMessage.mockReset()
    onReady.mockReset()
    isRemote.mockReset().mockReturnValue(false)
  })

  it("keeps the prompt and due time and deletes the source only after confirmation", async () => {
    const { id, scopeId, draft, scheduledAt, status, runtimeId } =
      saveMessage("pending")
    let confirm!: (value: boolean) => void
    importMessage.mockReturnValue(
      new Promise<boolean>((resolve) => {
        confirm = resolve
      })
    )
    const view = render(<ScheduledMessageMigration />)
    expect(importMessage).toHaveBeenCalledExactlyOnceWith({
      id,
      scopeId,
      draft,
      scheduledAt,
      status,
    })
    expect(localStorage.getItem(MESSAGE_PREFIX + id)).not.toBeNull()
    expect(localStorage.getItem(RUNTIME_PREFIX + runtimeId)).not.toBeNull()

    await act(async () => confirm(true))
    expect(localStorage.getItem(MESSAGE_PREFIX + id)).toBeNull()
    expect(localStorage.getItem(RUNTIME_PREFIX + runtimeId)).toBeNull()
    view.unmount()
    render(<ScheduledMessageMigration />)
    expect(importMessage).toHaveBeenCalledTimes(1)
  })

  it("retains a failed upload and retries it with the same identity on reconnect", async () => {
    saveMessage("retry")
    importMessage.mockRejectedValueOnce(new Error("connection lost"))
    importMessage.mockResolvedValueOnce(true)
    await act(async () => {
      render(<ScheduledMessageMigration />)
    })
    const ready = onReady.mock.calls[0][0] as () => void
    expect(localStorage.getItem(MESSAGE_PREFIX + "retry")).not.toBeNull()
    expect(localStorage.getItem(RUNTIME_PREFIX + "runtime")).not.toBeNull()
    await act(async () => ready())
    expect(importMessage).toHaveBeenCalledTimes(2)
    expect(importMessage.mock.calls[1]).toEqual(importMessage.mock.calls[0])
    expect(localStorage.length).toBe(0)
  })

  it("keeps invalid and unresolved sources and their shared heartbeat", async () => {
    saveMessage("draft-scope", { scopeId: "draft:42" })
    saveMessage("negative-scope", { scopeId: "conversation:-42" })
    saveMessage("mismatched-key", { id: "not-the-key" })
    const unresolved = saveMessage("unresolved", { scopeId: "conversation:99" })
    saveMessage("confirmed", { status: "failed" })
    importMessage.mockResolvedValueOnce(false)
    importMessage.mockResolvedValueOnce(true)
    render(<ScheduledMessageMigration />)
    await waitFor(() =>
      expect(localStorage.getItem(MESSAGE_PREFIX + "confirmed")).toBeNull()
    )
    expect(importMessage).toHaveBeenCalledTimes(2)
    expect(importMessage.mock.calls[0][0]).toMatchObject({
      id: unresolved.id,
      scopeId: unresolved.scopeId,
    })
    expect(importMessage.mock.calls[1][0]).toMatchObject({ status: "failed" })
    for (const id of [
      "draft-scope",
      "negative-scope",
      "mismatched-key",
      "unresolved",
    ]) {
      expect(localStorage.getItem(MESSAGE_PREFIX + id)).not.toBeNull()
    }
    expect(localStorage.getItem(RUNTIME_PREFIX + "runtime")).not.toBeNull()
  })

  it("keeps shared desktop storage untouched in a remote window", async () => {
    saveMessage("ambiguous")
    isRemote.mockReturnValue(true)
    await act(async () => {
      render(<ScheduledMessageMigration />)
    })
    expect(importMessage).not.toHaveBeenCalled()
    expect(localStorage.getItem(MESSAGE_PREFIX + "ambiguous")).not.toBeNull()
    expect(localStorage.getItem(RUNTIME_PREFIX + "runtime")).not.toBeNull()
  })
})
