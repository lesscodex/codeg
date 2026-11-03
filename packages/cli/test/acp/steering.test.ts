import { describe, expect, test } from "bun:test"
import type { SessionNotification } from "@agentclientprotocol/sdk"
import { delivered, durableEvent, rpcError, startSession, startWire, succeeded, textDelta } from "./wire-fixture"

const chunkText = (item: SessionNotification) =>
  item.update.sessionUpdate === "agent_message_chunk" && item.update.content.type === "text"
    ? item.update.content.text
    : undefined

describe("ACP steering", () => {
  test("advertises the top-level steering extension", async () => {
    await using acp = await startWire()
    expect((await acp.initialize())._meta).toEqual({ steering: { supported: true } })
  })

  test("injects into the active OpenCode execution and keeps the owning prompt open", async () => {
    await using acp = await startSession({
      onPrompt: ({ sessionID, id }) =>
        acp.server.prompts.length === 1
          ? [delivered(sessionID, id), textDelta(sessionID, "msg_initial", "working")]
          : [delivered(sessionID, id), textDelta(sessionID, "msg_steer", "redirected"), succeeded(sessionID)],
    })

    let settled = false
    const owner = acp.prompt(acp.sessionId, "initial").finally(() => (settled = true))
    await acp.waitForUpdate((item) => chunkText(item) === "working", "the initial prompt is active")
    expect(settled).toBe(false)

    expect(await acp.steer(acp.sessionId, "change direction")).toEqual({ outcome: "injected" })
    expect((await owner).stopReason).toBe("end_turn")
    expect(acp.server.prompts.map((item) => item.text)).toEqual(["initial", "change direction"])
  })

  test("keeps assistant-only steering context synthetic", async () => {
    await using acp = await startSession({
      onPrompt: ({ sessionID, id }) =>
        acp.server.prompts.length === 1
          ? [delivered(sessionID, id), textDelta(sessionID, "msg_initial", "working")]
          : [delivered(sessionID, id), succeeded(sessionID)],
    })

    const owner = acp.prompt(acp.sessionId, "initial")
    await acp.waitForUpdate((item) => chunkText(item) === "working")
    expect(
      await acp.steer(acp.sessionId, [
        { type: "text", text: "hidden context", annotations: { audience: ["assistant"] } },
        { type: "text", text: "visible redirect" },
      ]),
    ).toEqual({ outcome: "injected" })
    expect((await owner).stopReason).toBe("end_turn")
    expect(acp.server.submissions).toEqual([
      expect.objectContaining({ kind: "prompt", text: "initial" }),
      {
        kind: "synthetic",
        sessionID: acp.sessionId,
        text: "hidden context",
        description: "ACP embedded context",
        delivery: "steer",
        resume: false,
      },
      expect.objectContaining({ kind: "prompt", text: "visible redirect", files: [] }),
    ])
  })

  test("returns promptRequired while idle without admitting input", async () => {
    await using acp = await startSession()
    expect(await acp.steer(acp.sessionId, "late")).toEqual({
      outcome: "promptRequired",
      reason: "noRunningTurn",
    })
    expect(acp.server.prompts).toEqual([])
  })

  test("cancels an admitted steer when turn end wins the race", async () => {
    await using acp = await startSession({
      onPrompt: ({ sessionID, id }) =>
        acp.server.prompts.length === 1
          ? [delivered(sessionID, id), textDelta(sessionID, "msg_initial", "working")]
          : [],
    })
    const owner = acp.prompt(acp.sessionId, "initial")
    await acp.waitForUpdate((item) => chunkText(item) === "working")
    const steering = acp.steer(acp.sessionId, "too late")
    await acp.until(() => acp.server.prompts.length === 2, "steering admission")
    acp.server.send(succeeded(acp.sessionId))

    expect(await steering).toEqual({ outcome: "promptRequired", reason: "noRunningTurn" })
    expect((await owner).stopReason).toBe("end_turn")
    expect(acp.server.cancelledInbox).toHaveLength(1)
  })

  test("keeps the owner open when delivery wins the turn-end cancellation race", async () => {
    await using acp = await startSession({
      onPrompt: ({ sessionID, id }) =>
        acp.server.prompts.length === 1
          ? [delivered(sessionID, id), textDelta(sessionID, "msg_initial", "working")]
          : [],
      onInboxCancel: ({ sessionID, inboxID }) => [delivered(sessionID, inboxID), succeeded(sessionID)],
    })
    const owner = acp.prompt(acp.sessionId, "initial")
    await acp.waitForUpdate((item) => chunkText(item) === "working")
    const steering = acp.steer(acp.sessionId, "race")
    await acp.until(() => acp.server.prompts.length === 2, "steering admission")
    acp.server.send(succeeded(acp.sessionId))

    expect(await steering).toEqual({ outcome: "injected" })
    expect((await owner).stopReason).toBe("end_turn")
  })

  test("waits for a real inbox outcome when the cancellation request fails", async () => {
    await using acp = await startSession({
      fetch: (request) =>
        request.method === "DELETE" && request.path.includes("/inbox/")
          ? new Response(null, { status: 500 })
          : undefined,
      onPrompt: ({ sessionID, id }) =>
        acp.server.prompts.length === 1
          ? [delivered(sessionID, id), textDelta(sessionID, "msg_initial", "working")]
          : [],
    })
    const owner = acp.prompt(acp.sessionId, "initial")
    await acp.waitForUpdate((item) => chunkText(item) === "working")
    const steering = acp.steer(acp.sessionId, "race")
    await acp.until(() => acp.server.prompts.length === 2, "steering admission")
    const admitted = acp.server.prompts[1]
    expect(admitted).toBeDefined()
    const inboxID = admitted!.id
    acp.server.send(succeeded(acp.sessionId))
    await acp.until(
      () => acp.server.requests.find((request) => request.method === "DELETE" && request.path.includes(inboxID)),
      "failed cancellation request",
    )
    acp.server.send(delivered(acp.sessionId, inboxID), succeeded(acp.sessionId))

    expect(await steering).toEqual({ outcome: "injected" })
    expect((await owner).stopReason).toBe("end_turn")
  })

  test("ignores unrelated inbox events while waiting for the accepted steer outcome", async () => {
    await using acp = await startSession({
      onPrompt: ({ sessionID, id }) =>
        acp.server.prompts.length === 1
          ? [delivered(sessionID, id), textDelta(sessionID, "msg_initial", "working")]
          : [],
      onInboxCancel: () => [],
    })
    const owner = acp.prompt(acp.sessionId, "initial")
    await acp.waitForUpdate((item) => chunkText(item) === "working")
    const steering = acp.steer(acp.sessionId, "late")
    await acp.until(() => acp.server.prompts.length === 2, "steering admission")
    const admitted = acp.server.prompts[1]
    expect(admitted).toBeDefined()
    const inboxID = admitted!.id
    acp.server.send(succeeded(acp.sessionId))
    await acp.until(() => acp.server.cancelledInbox.length === 1, "cancellation admission")
    acp.server.send(
      delivered(acp.sessionId, "msg_unrelated"),
      durableEvent("session.inbox.cancelled", { sessionID: acp.sessionId, inboxID }),
    )

    expect(await steering).toEqual({ outcome: "promptRequired", reason: "noRunningTurn" })
    expect((await owner).stopReason).toBe("end_turn")
  })

  test("rejects unsupported content before admission", async () => {
    await using acp = await startSession()
    expect(
      await rpcError(acp.steer(acp.sessionId, [{ type: "video", uri: "file:///tmp/video" } as never])),
    ).toMatchObject({ code: -32600 })
    expect(acp.server.prompts).toEqual([])
  })
})
