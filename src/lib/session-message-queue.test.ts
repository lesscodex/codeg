import { describe, expect, it, vi } from "vitest"
import { acceptSessionMessageQueued } from "./session-message-queue"

const payload = {
  request_id: "handoff-1",
  conversation_id: 42,
  message: "[Forwarded through MCP] hello",
}

describe("acceptSessionMessageQueued", () => {
  it("enqueues through the existing queue and acknowledges only after acceptance", async () => {
    const order: string[] = []
    const enqueue = vi.fn(() => order.push("enqueue"))
    const acknowledge = vi.fn(() => {
      order.push("ack")
      return Promise.resolve(true)
    })

    expect(
      acceptSessionMessageQueued(payload, 42, false, enqueue, acknowledge)
    ).toBe(true)
    expect(order).toEqual(["enqueue", "ack"])
    expect(enqueue).toHaveBeenCalledWith(
      expect.objectContaining({ displayText: payload.message }),
      null
    )
    expect(acknowledge).toHaveBeenCalledWith("handoff-1", 42)
  })

  it("does not acknowledge when no matching target page can receive it", () => {
    const enqueue = vi.fn()
    const acknowledge = vi.fn(() => Promise.resolve(true))

    expect(
      acceptSessionMessageQueued(payload, 99, false, enqueue, acknowledge)
    ).toBe(false)
    expect(enqueue).not.toHaveBeenCalled()
    expect(acknowledge).not.toHaveBeenCalled()
  })

  it("does not acknowledge from a viewer surface", () => {
    const enqueue = vi.fn()
    const acknowledge = vi.fn(() => Promise.resolve(true))

    expect(
      acceptSessionMessageQueued(payload, 42, true, enqueue, acknowledge)
    ).toBe(false)
    expect(enqueue).not.toHaveBeenCalled()
    expect(acknowledge).not.toHaveBeenCalled()
  })

  it("fails closed when a late event has no active subscription handler", () => {
    const enqueue = vi.fn()
    const acknowledge = vi.fn(() => Promise.resolve(true))

    let receiver: ((event: typeof payload) => void) | undefined
    const event = Promise.resolve().then(() => receiver?.(payload))
    const subscription = Promise.resolve().then(() => {
      receiver = (eventPayload) => {
        acceptSessionMessageQueued(
          eventPayload,
          42,
          false,
          enqueue,
          acknowledge
        )
      }
    })

    return Promise.all([event, subscription]).then(() => {
      expect(enqueue).not.toHaveBeenCalled()
      expect(acknowledge).not.toHaveBeenCalled()
    })
  })
})
