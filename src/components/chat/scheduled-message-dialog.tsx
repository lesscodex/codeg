"use client"

import { useState } from "react"
import { useTranslations } from "next-intl"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import {
  parseDateTimeLocalValue,
  toDateTimeLocalValue,
} from "@/lib/task-schedule"

interface ScheduledMessageDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  initialScheduledAt?: number | null
  preview?: string
  onConfirm: (scheduledAt: number) => boolean
}

function defaultScheduledAt(now: number): number {
  const date = new Date(now)
  date.setMinutes(0, 0, 0)
  date.setHours(date.getHours() + 1)
  return date.getTime()
}

export function ScheduledMessageDialog({
  open,
  onOpenChange,
  initialScheduledAt,
  preview,
  onConfirm,
}: ScheduledMessageDialogProps) {
  if (!open) return null
  return (
    <OpenScheduledMessageDialog
      onOpenChange={onOpenChange}
      initialScheduledAt={initialScheduledAt}
      preview={preview}
      onConfirm={onConfirm}
    />
  )
}

function OpenScheduledMessageDialog({
  onOpenChange,
  initialScheduledAt,
  preview,
  onConfirm,
}: Omit<ScheduledMessageDialogProps, "open">) {
  const t = useTranslations("Folder.chat.scheduledMessages")
  const [openedAt] = useState(() => Date.now())
  const [value, setValue] = useState(() =>
    toDateTimeLocalValue(
      new Date(initialScheduledAt ?? defaultScheduledAt(openedAt))
    )
  )
  const minValue = toDateTimeLocalValue(new Date(openedAt + 60_000))
  const [invalid, setInvalid] = useState(false)

  const confirm = () => {
    const date = parseDateTimeLocalValue(value)
    if (!date || date.getTime() <= Date.now()) {
      setInvalid(true)
      return
    }
    if (onConfirm(date.getTime())) onOpenChange(false)
  }

  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{t("dialogTitle")}</DialogTitle>
          <DialogDescription>{t("dialogDescription")}</DialogDescription>
        </DialogHeader>
        {preview && (
          <div className="truncate rounded-md border bg-muted/40 px-3 py-2 text-sm text-muted-foreground">
            {preview}
          </div>
        )}
        <div className="space-y-1.5">
          <Input
            type="datetime-local"
            aria-label={t("dialogTitle")}
            value={value}
            min={minValue}
            onChange={(event) => {
              setValue(event.target.value)
              setInvalid(false)
            }}
            aria-invalid={invalid}
          />
          {invalid && (
            <p className="text-xs text-destructive">
              {t("futureTimeRequired")}
            </p>
          )}
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            {t("cancel")}
          </Button>
          <Button onClick={confirm}>{t("confirm")}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
