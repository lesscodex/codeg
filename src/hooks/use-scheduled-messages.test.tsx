import { act, renderHook } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import type { PromptDraft } from "@/lib/types"
import { useScheduledMessages } from "@/hooks/use-scheduled-messages"

const draft: PromptDraft = {
  blocks: [{ type: "text", text: "send later" }],
  displayText: "send later",
}

describe("useScheduledMessages", () => {
  beforeEach(() => {
    vi.useFakeTimers()
    vi.setSystemTime(new Date("2026-10-13T08:00:00Z"))
    window.localStorage.clear()
  })

  afterEach(() => {
    vi.useRealTimers()
  })

  it("delivers a due message through the existing send callback", async () => {
    const deliver = vi.fn((_draft, callbacks) => callbacks.accepted())
    const { result } = renderHook(() =>
      useScheduledMessages({
        scopeId: "tab-a",
        connectionReady: true,
        prompting: false,
        deliver,
      })
    )

    act(() => {
      result.current.schedule(draft, Date.now() + 1_000)
    })
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000)
    })

    expect(deliver).toHaveBeenCalledWith(
      draft,
      expect.objectContaining({ accepted: expect.any(Function) })
    )
    expect(result.current.messages).toEqual([])
  })

  it("keeps a due message waiting until the target conversation is idle", async () => {
    const deliver = vi.fn((_draft, callbacks) => callbacks.accepted())
    const { result, rerender } = renderHook(
      ({ prompting }) =>
        useScheduledMessages({
          scopeId: "tab-a",
          connectionReady: !prompting,
          prompting,
          deliver,
        }),
      { initialProps: { prompting: true } }
    )

    act(() => {
      result.current.schedule(draft, Date.now() + 1_000)
    })
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000)
    })
    expect(deliver).not.toHaveBeenCalled()
    expect(result.current.messages[0]?.status).toBe("waiting")

    rerender({ prompting: false })
    expect(deliver).toHaveBeenCalledWith(
      draft,
      expect.objectContaining({ accepted: expect.any(Function) })
    )
    expect(result.current.messages).toEqual([])
  })

  it("asks a disconnected target conversation to reconnect", async () => {
    const ensureReady = vi.fn()
    const { result } = renderHook(() =>
      useScheduledMessages({
        scopeId: "tab-a",
        connectionReady: false,
        prompting: false,
        deliver: vi.fn(),
        ensureReady,
      })
    )

    act(() => {
      result.current.schedule(draft, Date.now() + 1_000)
    })
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000)
    })

    expect(ensureReady).toHaveBeenCalledTimes(1)
    expect(result.current.messages[0]?.status).toBe("waiting")
  })

  it("retains a rejected delivery as failed instead of losing it", async () => {
    const deliver = vi.fn((_draft, callbacks) => callbacks.failed())
    const { result } = renderHook(() =>
      useScheduledMessages({
        scopeId: "tab-a",
        connectionReady: true,
        prompting: false,
        deliver,
      })
    )

    act(() => {
      result.current.schedule(draft, Date.now() + 1_000)
    })
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000)
    })

    expect(result.current.messages[0]).toMatchObject({ status: "failed" })
  })

  it("retries a busy target without moving the message into the volatile queue", async () => {
    const deliver = vi
      .fn()
      .mockImplementationOnce((_draft, callbacks) => callbacks.busy())
      .mockImplementationOnce((_draft, callbacks) => callbacks.accepted())
    const { result } = renderHook(() =>
      useScheduledMessages({
        scopeId: "tab-a",
        connectionReady: true,
        prompting: false,
        deliver,
      })
    )

    act(() => {
      result.current.schedule(draft, Date.now() + 1_000)
    })
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000)
    })
    expect(deliver).toHaveBeenCalledTimes(1)
    expect(result.current.messages[0]?.status).toBe("waiting")

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000)
    })
    expect(deliver).toHaveBeenCalledTimes(2)
    expect(result.current.messages).toEqual([])
  })

  it("claims a due message once when the same conversation is mounted twice", async () => {
    let accept: (() => void) | undefined
    const deliver = vi.fn((_draft, callbacks) => {
      accept = callbacks.accepted
    })
    const first = renderHook(() =>
      useScheduledMessages({
        scopeId: "tab-a",
        connectionReady: true,
        prompting: false,
        deliver,
      })
    )
    const second = renderHook(() =>
      useScheduledMessages({
        scopeId: "tab-a",
        connectionReady: true,
        prompting: false,
        deliver,
      })
    )

    act(() => {
      first.result.current.schedule(draft, Date.now() + 1_000)
    })
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000)
    })
    expect(deliver).toHaveBeenCalledTimes(1)

    act(() => accept?.())
    expect(first.result.current.messages).toEqual([])
    expect(second.result.current.messages).toEqual([])
  })

  it("does not deliver when another browser page owns the delivery lock", async () => {
    const descriptor = Object.getOwnPropertyDescriptor(navigator, "locks")
    const request = vi.fn(
      (
        _name: string,
        _options: LockOptions,
        callback: (lock: Lock | null) => void | Promise<void>
      ) => Promise.resolve(callback(null))
    )
    Object.defineProperty(navigator, "locks", {
      configurable: true,
      value: { request },
    })
    const deliver = vi.fn()

    try {
      const { result } = renderHook(() =>
        useScheduledMessages({
          scopeId: "tab-a",
          connectionReady: true,
          prompting: false,
          deliver,
        })
      )
      act(() => {
        result.current.schedule(draft, Date.now() + 1_000)
      })
      await act(async () => {
        await vi.advanceTimersByTimeAsync(1_000)
      })

      expect(request).toHaveBeenCalledOnce()
      expect(deliver).not.toHaveBeenCalled()
    } finally {
      if (descriptor) {
        Object.defineProperty(navigator, "locks", descriptor)
      } else {
        Reflect.deleteProperty(navigator, "locks")
      }
    }
  })
})
