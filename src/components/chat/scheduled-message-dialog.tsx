"use client"

import { useId, useState } from "react"
import { useLocale, useTranslations } from "next-intl"
import { ChevronDown } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Calendar } from "@/components/ui/calendar"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover"
import { dateFnsLocale } from "@/lib/date-fns-locale"
import {
  formatScheduleDay,
  joinDateAndTime,
  parseDateTimeLocalValue,
  splitDateTimeLocalValue,
  toDateTimeLocalValue,
} from "@/lib/task-schedule"

interface ScheduledMessageDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  initialScheduledAt?: number | null
  preview?: string
  onConfirm: (scheduledAt: number) => boolean | Promise<boolean>
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
  const tSchedule = useTranslations("Tasks")
  const locale = useLocale()
  const id = useId()
  const [openedAt] = useState(() => Date.now())
  const [{ date, time }, setDateTime] = useState(() =>
    splitDateTimeLocalValue(
      toDateTimeLocalValue(
        new Date(initialScheduledAt ?? defaultScheduledAt(openedAt))
      )
    )
  )
  const selectedDay = new Date(`${date}T00:00`)
  const min = splitDateTimeLocalValue(
    toDateTimeLocalValue(new Date(openedAt + 60_000))
  )
  const [calendarOpen, setCalendarOpen] = useState(false)
  const [invalid, setInvalid] = useState(false)
  const [confirming, setConfirming] = useState(false)

  const confirm = async () => {
    if (confirming) return
    const scheduledAt = parseDateTimeLocalValue(joinDateAndTime(date, time))
    if (!scheduledAt || scheduledAt.getTime() <= Date.now()) {
      setInvalid(true)
      return
    }
    setConfirming(true)
    try {
      if (await onConfirm(scheduledAt.getTime())) onOpenChange(false)
    } finally {
      setConfirming(false)
    }
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
          <div className="flex items-end gap-2">
            <div className="flex min-w-0 flex-1 flex-col gap-1.5">
              <Label htmlFor={`${id}-date`}>
                {tSchedule("scheduleDateLabel")}
              </Label>
              <Popover open={calendarOpen} onOpenChange={setCalendarOpen}>
                <PopoverTrigger asChild>
                  <Button
                    id={`${id}-date`}
                    type="button"
                    variant="outline"
                    className="h-9 w-full justify-between px-3 font-normal"
                    aria-invalid={invalid}
                  >
                    <span className="truncate">
                      {formatScheduleDay(date, locale)}
                    </span>
                    <ChevronDown
                      className="size-4 shrink-0 opacity-50"
                      aria-hidden="true"
                    />
                  </Button>
                </PopoverTrigger>
                <PopoverContent align="start" className="w-auto p-0">
                  <Calendar
                    mode="single"
                    autoFocus
                    locale={dateFnsLocale(locale)}
                    selected={selectedDay}
                    defaultMonth={selectedDay}
                    disabled={{ before: new Date(`${min.date}T00:00`) }}
                    onSelect={(picked) => {
                      if (!picked) return
                      setDateTime({
                        date: toDateTimeLocalValue(picked).slice(0, 10),
                        time,
                      })
                      setCalendarOpen(false)
                      setInvalid(false)
                    }}
                  />
                </PopoverContent>
              </Popover>
            </div>
            <div className="flex w-32 shrink-0 flex-col gap-1.5">
              <Label htmlFor={`${id}-time`}>
                {tSchedule("scheduleTimeLabel")}
              </Label>
              <Input
                id={`${id}-time`}
                type="time"
                value={time}
                min={date === min.date ? min.time : undefined}
                onChange={(event) => {
                  setDateTime({ date, time: event.target.value })
                  setInvalid(false)
                }}
                aria-invalid={invalid}
                className="px-3 [&::-webkit-calendar-picker-indicator]:hidden"
              />
            </div>
          </div>
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
          <Button onClick={() => void confirm()} disabled={confirming}>
            {t("confirm")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
