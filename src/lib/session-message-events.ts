export const SESSION_MESSAGE_QUEUED_EVENT = "session-message://queued"

export interface SessionMessageQueuedEvent {
  request_id: string
  conversation_id: number
  message: string
}
