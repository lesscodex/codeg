import { act, renderHook } from "@testing-library/react"
import { beforeEach, expect, it, vi } from "vitest"
import { useScheduledMessages } from "@/hooks/use-scheduled-messages"
import {
  listScheduledMessages,
  SCHEDULED_MESSAGE_CHANGED_EVENT,
} from "@/lib/api"
import { onTransportReconnect, subscribe } from "@/lib/platform"
import type { ScheduledMessage } from "@/lib/types"

vi.mock("@/lib/api", () => ({
  listScheduledMessages: vi.fn(),
  SCHEDULED_MESSAGE_CHANGED_EVENT: "scheduled-message://changed",
}))
vi.mock("@/lib/platform", () => ({
  onTransportReconnect: vi.fn(),
  subscribe: vi.fn(),
}))

const mockList = vi.mocked(listScheduledMessages)
const message: ScheduledMessage = {
  id: "message-1",
  conversationId: 42,
  draft: {
    blocks: [{ type: "text", text: "send later" }],
    displayText: "send later",
  },
  scheduledAt: 1500,
  status: "scheduled",
  error: null,
}

beforeEach(() => {
  vi.resetAllMocks()
  vi.mocked(subscribe).mockResolvedValue(() => {})
  mockList.mockResolvedValue([])
})

it("loads only for a persisted conversation and follows backend status events", async () => {
  mockList.mockResolvedValueOnce([message])
  const { result, rerender } = renderHook(
    ({ id }: { id: number | null }) => useScheduledMessages(id),
    { initialProps: { id: null as number | null } }
  )
  expect(mockList).not.toHaveBeenCalled()
  await act(async () => rerender({ id: 42 }))
  expect(mockList).toHaveBeenCalledExactlyOnceWith(42)
  expect(result.current).toEqual([message])

  const [channel, refresh] = vi.mocked(subscribe).mock.calls[0]
  expect(channel).toBe(SCHEDULED_MESSAGE_CHANGED_EVENT)
  mockList.mockResolvedValueOnce([{ ...message, status: "waiting" }])
  await act(async () => refresh(null))
  expect(result.current[0]?.status).toBe("waiting")

  const [onReconnect] = vi.mocked(onTransportReconnect).mock.calls[0]
  await act(async () => onReconnect())
  expect(result.current).toEqual([])
})

it("ignores old results when switching conversations", async () => {
  let finish!: (messages: ScheduledMessage[]) => void
  mockList.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        finish = resolve
      })
  )
  const { result, rerender } = renderHook(useScheduledMessages, {
    initialProps: 42,
  })
  await act(async () => {})
  await act(async () => rerender(43))
  await act(async () => finish([message]))
  expect(mockList.mock.calls).toEqual([[42], [43]])
  expect(result.current).toEqual([])
})
