import { beforeEach, describe, expect, it, vi } from "vitest"

const mocks = vi.hoisted(() => ({ call: vi.fn() }))

vi.mock("@/lib/transport", () => ({
  getTransport: () => ({ call: mocks.call }),
  getShellTransport: () => ({ call: vi.fn() }),
  isDesktop: () => false,
  isRemoteDesktopMode: () => false,
  getActiveRemoteConnectionId: () => null,
  notifyRemoteDesktopUnauthorized: vi.fn(),
}))

import { acpFork, acpForkAtTurn } from "@/lib/api"

describe("acpFork", () => {
  beforeEach(() => mocks.call.mockReset())

  it("passes the optional native Codex fork point without changing the result", async () => {
    const result = {
      forkedSessionId: "session-2",
      originalSessionId: "session-1",
      siblingConversationId: 42,
    }
    mocks.call.mockResolvedValue(result)

    await expect(
      acpFork("connection-1", 7, 3, "codex-turn-17")
    ).resolves.toEqual(result)
    expect(mocks.call).toHaveBeenCalledWith("acp_fork", {
      connectionId: "connection-1",
      conversationId: 7,
      folderId: 3,
      nativeTurnId: "codex-turn-17",
    })
  })

  it("sends the historical flag through the standard fork command", async () => {
    const result = {
      forkedSessionId: "session-2",
      originalSessionId: "session-1",
      forkedConversationId: 43,
      forkedConversationTitle: "[Fork] Topic",
    }
    mocks.call.mockResolvedValue(result)

    await expect(
      acpFork("connection-1", 7, 3, "codex-turn-17", "turn-17", true)
    ).resolves.toEqual(result)
    expect(mocks.call).toHaveBeenCalledWith("acp_fork", {
      connectionId: "connection-1",
      conversationId: 7,
      folderId: 3,
      nativeTurnId: "codex-turn-17",
      forkFromTurnId: "turn-17",
      historicalFork: true,
    })
  })

  it("keeps the historical fingerprint point when no native UUID exists", async () => {
    mocks.call.mockResolvedValue({
      forkedSessionId: "session-2",
      originalSessionId: "session-1",
      forkedConversationId: 43,
      forkedConversationTitle: "[Fork] Topic",
    })

    await acpFork("connection-1", 7, 3, null, "turn-17", true)

    expect(mocks.call).toHaveBeenCalledWith("acp_fork", {
      connectionId: "connection-1",
      conversationId: 7,
      folderId: 3,
      forkFromTurnId: "turn-17",
      historicalFork: true,
    })
  })

  it("keeps the existing fork-send payload when no fork point is supplied", async () => {
    mocks.call.mockResolvedValue({
      forkedSessionId: "session-2",
      originalSessionId: "session-1",
      siblingConversationId: 42,
    })

    await acpFork("connection-1", 7, 3)

    expect(mocks.call).toHaveBeenCalledWith("acp_fork", {
      connectionId: "connection-1",
      conversationId: 7,
      folderId: 3,
    })
  })

  it("keeps rendered fork points separate from native Codex turn IDs", async () => {
    mocks.call.mockResolvedValue({
      forkedSessionId: "session-2",
      originalSessionId: "session-1",
      siblingConversationId: 42,
    })

    await acpFork("connection-1", 7, 3, null, "turn-17")

    expect(mocks.call).toHaveBeenCalledWith("acp_fork", {
      connectionId: "connection-1",
      conversationId: 7,
      folderId: 3,
      forkFromTurnId: "turn-17",
    })
  })
})

describe("acpForkAtTurn", () => {
  beforeEach(() => mocks.call.mockReset())

  it("uses the independent historical-fork endpoint and returns its target row", async () => {
    const result = {
      forkedSessionId: "session-2",
      originalSessionId: "session-1",
      forkedConversationId: 43,
      forkedConversationTitle: "[Fork] Topic",
    }
    mocks.call.mockResolvedValue(result)

    await expect(
      acpForkAtTurn("connection-1", 7, 3, "codex-turn-17")
    ).resolves.toEqual(result)
    expect(mocks.call).toHaveBeenCalledWith("acp_fork_at_turn", {
      connectionId: "connection-1",
      conversationId: 7,
      folderId: 3,
      nativeTurnId: "codex-turn-17",
    })
  })
})
