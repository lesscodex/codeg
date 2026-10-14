"use client"

import { useState } from "react"
import { useTranslations } from "next-intl"
import { CalendarClock, ClockAlert, Pencil, X } from "lucide-react"
import { ScheduledMessageDialog } from "@/components/chat/scheduled-message-dialog"
import { cn } from "@/lib/utils"
import { formatScheduleShort } from "@/lib/task-schedule"
import type { ScheduledMessage } from "@/lib/scheduled-messages"

interface ScheduledMessageDisplayProps {
  messages: ScheduledMessage[]
  onReschedule: (id: string, scheduledAt: number) => boolean
  onRemove: (id: string) => void
}

export function ScheduledMessageDisplay({
  messages,
  onReschedule,
  onRemove,
}: ScheduledMessageDisplayProps) {
  const t = useTranslations("Folder.chat.scheduledMessages")
  const [editing, setEditing] = useState<ScheduledMessage | null>(null)

  if (messages.length === 0) return null

  return (
    <>
      <div className="mb-1 flex max-h-24 flex-col gap-1 overflow-y-auto rounded-md border bg-muted/20 p-1">
        {messages.map((item) => {
          const failed = item.status === "failed"
          const missed = item.status === "missed" || failed
          return (
            <div
              key={item.id}
              className={cn(
                "flex items-center gap-1 rounded-md border px-1.5 py-1 text-3xs leading-none",
                missed
                  ? "border-destructive/30 bg-destructive/5"
                  : "border-border/70 bg-muted/40"
              )}
            >
              {missed ? (
                <ClockAlert className="size-3 shrink-0 text-destructive" />
              ) : (
                <CalendarClock className="size-3 shrink-0 text-muted-foreground" />
              )}
              <span className="min-w-0 flex-1 truncate text-foreground/80">
                {item.draft.displayText}
              </span>
              <span
                className={cn(
                  "shrink-0 text-muted-foreground",
                  missed && "text-destructive"
                )}
              >
                {missed
                  ? t(failed ? "failed" : "missed")
                  : item.status === "waiting"
                    ? t("waiting")
                    : formatScheduleShort(
                        new Date(item.scheduledAt).toISOString()
                      )}
              </span>
              <button
                type="button"
                onClick={() => setEditing(item)}
                className="shrink-0 rounded-sm p-0.5 text-muted-foreground hover:bg-muted-foreground/15"
                title={t("editTime")}
              >
                <Pencil className="size-2.5" />
              </button>
              <button
                type="button"
                onClick={() => onRemove(item.id)}
                className="shrink-0 rounded-sm p-0.5 text-muted-foreground hover:bg-muted-foreground/15"
                title={t("remove")}
              >
                <X className="size-2.5" />
              </button>
            </div>
          )
        })}
      </div>
      <ScheduledMessageDialog
        open={editing != null}
        onOpenChange={(open) => {
          if (!open) setEditing(null)
        }}
        initialScheduledAt={editing?.scheduledAt}
        preview={editing?.draft.displayText}
        onConfirm={(scheduledAt) =>
          editing ? onReschedule(editing.id, scheduledAt) : false
        }
      />
    </>
  )
}
