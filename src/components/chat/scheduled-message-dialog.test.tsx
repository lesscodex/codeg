import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { NextIntlClientProvider } from "next-intl"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import enMessages from "@/i18n/messages/en.json"
import { formatScheduleDay } from "@/lib/task-schedule"
import { ScheduledMessageDialog } from "./scheduled-message-dialog"

function renderDialog(initialScheduledAt?: number) {
  const onConfirm = vi.fn(() => true)
  render(
    <NextIntlClientProvider locale="en" messages={enMessages}>
      <ScheduledMessageDialog
        open
        onOpenChange={vi.fn()}
        initialScheduledAt={initialScheduledAt}
        onConfirm={onConfirm}
      />
    </NextIntlClientProvider>
  )
  return onConfirm
}

const dateTrigger = () =>
  screen.getByLabelText(enMessages.Tasks.scheduleDateLabel)
const timeInput = () =>
  screen.getByLabelText(enMessages.Tasks.scheduleTimeLabel)
const confirmButton = () =>
  screen.getByRole("button", {
    name: enMessages.Folder.chat.scheduledMessages.confirm,
  })

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["Date"] })
  vi.setSystemTime(new Date(2026, 10, 28, 21, 37))
})

afterEach(() => {
  cleanup()
  vi.useRealTimers()
})

describe("ScheduledMessageDialog", () => {
  it.each([
    { hour: 21, day: "2026-11-28", time: "22:00" },
    { hour: 23, day: "2026-11-29", time: "00:00" },
  ])("defaults to the next full hour at $hour:37", ({ hour, day, time }) => {
    vi.setSystemTime(new Date(2026, 10, 28, hour, 37))
    renderDialog()

    expect(dateTrigger()).toHaveTextContent(formatScheduleDay(day, "en"))
    expect(timeInput()).toHaveValue(time)
  })

  it("keeps the existing date while the time is cleared and re-entered", async () => {
    const user = userEvent.setup()
    const onConfirm = renderDialog(new Date(2026, 10, 30, 9, 45).getTime())
    expect(dateTrigger()).toHaveTextContent(
      formatScheduleDay("2026-11-30", "en")
    )
    expect(timeInput()).toHaveValue("09:45")

    fireEvent.change(timeInput(), { target: { value: "" } })
    await user.click(confirmButton())
    expect(onConfirm).not.toHaveBeenCalled()
    expect(
      screen.getByText(
        enMessages.Folder.chat.scheduledMessages.futureTimeRequired
      )
    ).toBeVisible()

    fireEvent.change(timeInput(), { target: { value: "10:23" } })
    await user.click(confirmButton())
    expect(onConfirm).toHaveBeenCalledWith(
      new Date(2026, 10, 30, 10, 23).getTime()
    )
  })

  it("keeps the time when choosing another day and submits local time", async () => {
    const user = userEvent.setup()
    const onConfirm = renderDialog(new Date(2026, 10, 28, 22).getTime())
    fireEvent.change(timeInput(), { target: { value: "22:23" } })
    await user.click(dateTrigger())
    const tomorrow = new Date(2026, 10, 29)
    const day = await waitFor(() => {
      const button = document.querySelector<HTMLButtonElement>(
        `button[data-day="${tomorrow.toLocaleDateString()}"]`
      )
      if (!button) throw new Error("Calendar day is not available")
      return button
    })
    await user.click(day)

    expect(timeInput()).toHaveValue("22:23")
    await user.click(confirmButton())
    expect(onConfirm).toHaveBeenCalledWith(
      new Date(2026, 10, 29, 22, 23).getTime()
    )
  })

  it("rejects a past time on the selected day", async () => {
    const user = userEvent.setup()
    const onConfirm = renderDialog()
    fireEvent.change(timeInput(), { target: { value: "20:00" } })
    await user.click(confirmButton())

    expect(onConfirm).not.toHaveBeenCalled()
    expect(timeInput()).toHaveAttribute("aria-invalid", "true")
  })
})
