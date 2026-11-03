import { RequestError, type ContentBlock } from "@agentclientprotocol/sdk"

export const Method = "_session/steering"

export type Params = {
  readonly sessionId: string
  readonly prompt: ContentBlock[]
  readonly _meta?: { readonly steering?: { readonly idleBehavior?: "promptRequired" } }
}

export type Response =
  | { readonly outcome: "injected" }
  | { readonly outcome: "promptRequired"; readonly reason: "noRunningTurn" }

export const PromptRequired = { outcome: "promptRequired", reason: "noRunningTurn" } as const
export const Injected = { outcome: "injected" } as const

export function parse(value: unknown): Params {
  if (!record(value)) throw RequestError.invalidParams("steering params must be an object")
  if (typeof value.sessionId !== "string" || !value.sessionId)
    throw RequestError.invalidParams("sessionId must be a non-empty string")
  if (!Array.isArray(value.prompt)) throw RequestError.invalidParams("prompt must be an array of content blocks")
  for (const block of value.prompt) {
    if (!record(block) || typeof block.type !== "string")
      throw RequestError.invalidParams("prompt contains an invalid content block")
    if (!new Set(["text", "image", "audio", "resource", "resource_link"]).has(block.type))
      throw RequestError.invalidRequest(`unsupported steering content block: ${block.type}`)
  }
  if (value._meta !== undefined && value._meta !== null && !record(value._meta))
    throw RequestError.invalidParams("steering _meta must be an object")
  const meta = record(value._meta) ? value._meta : undefined
  const steeringValue = meta?.["steering"]
  if (steeringValue !== undefined && !record(steeringValue))
    throw RequestError.invalidParams("steering _meta.steering must be an object")
  const steering = record(steeringValue) ? steeringValue : undefined
  const idleBehavior = steering?.["idleBehavior"]
  if (idleBehavior !== undefined && idleBehavior !== "promptRequired")
    throw RequestError.invalidParams("unsupported steering idleBehavior")
  return {
    sessionId: value.sessionId,
    prompt: value.prompt as ContentBlock[],
    ...(idleBehavior === "promptRequired" ? { _meta: { steering: { idleBehavior: "promptRequired" as const } } } : {}),
  }
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value)
}

export * as ACPSteering from "./steering"
