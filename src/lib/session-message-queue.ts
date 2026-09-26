import type { SessionMessageQueuedEvent } from "@/lib/session-message-events"
import type { PromptDraft } from "@/lib/types"

export function acceptSessionMessageQueued(
  payload: SessionMessageQueuedEvent,
  conversationId: number | null,
  isViewer: boolean,
  enqueue: (draft: PromptDraft, modeId: string | null) => void,
  acknowledge: (requestId: string, conversationId: number) => Promise<unknown>
): boolean {
  if (
    isViewer ||
    !payload.request_id?.trim() ||
    payload.conversation_id !== conversationId ||
    !payload.message?.trim()
  ) {
    return false
  }

  const text = payload.message
  enqueue({ blocks: [{ type: "text", text }], displayText: text }, null)
  void acknowledge(payload.request_id, payload.conversation_id).catch(() => {})
  return true
}
