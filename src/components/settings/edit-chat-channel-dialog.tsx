"use client"

import { useCallback, useEffect, useId, useState } from "react"
import { Loader2 } from "lucide-react"
import { useTranslations } from "next-intl"
import { toast } from "sonner"

import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Switch } from "@/components/ui/switch"
import {
  updateChatChannel,
  saveChatChannelToken,
  getChatChannelHasToken,
} from "@/lib/api"
import type { ChatChannelInfo } from "@/lib/types"
import { toErrorMessage } from "@/lib/app-error"

interface EditChatChannelDialogProps {
  open: boolean
  channel: ChatChannelInfo
  onOpenChange: (open: boolean) => void
  onChannelUpdated: () => void
}

export function EditChatChannelDialog({
  open,
  channel,
  onOpenChange,
  onChannelUpdated,
}: EditChatChannelDialogProps) {
  const t = useTranslations("ChatChannelSettings")
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const config = JSON.parse(channel.config_json || "{}")
  const ownerOpenId =
    typeof config.owner_openid === "string" ? config.owner_openid : ""
  const configuredAppId =
    typeof config.app_id === "string" ? config.app_id.trim() : ""
  const [name, setName] = useState(channel.name)
  const [token, setToken] = useState("")
  const [chatId, setChatId] = useState(config.chat_id ?? "")
  const [appId, setAppId] = useState(config.app_id ?? "")
  const [baseUrl] = useState(config.base_url ?? "")
  const [topicMode, setTopicMode] = useState(Boolean(config.topic_mode))
  const [dailyReportEnabled, setDailyReportEnabled] = useState(
    channel.daily_report_enabled
  )
  const [dailyReportTime, setDailyReportTime] = useState(
    channel.daily_report_time || "18:00"
  )
  const [hasToken, setHasToken] = useState(false)
  const [resetOwnerBindingChannelId, setResetOwnerBindingChannelId] = useState<
    ChatChannelInfo["id"] | null
  >(null)
  const resetOwnerBinding = resetOwnerBindingChannelId === channel.id
  const topicModeId = useId()

  useEffect(() => {
    if (open) {
      getChatChannelHasToken(channel.id)
        .then(setHasToken)
        .catch(() => {})
    }
  }, [open, channel.id])

  const handleOpenChange = useCallback(
    (nextOpen: boolean) => {
      if (!nextOpen) setResetOwnerBindingChannelId(null)
      onOpenChange(nextOpen)
    },
    [onOpenChange]
  )

  const handleSubmit = useCallback(async () => {
    if (!name.trim()) {
      setError(t("nameRequired"))
      return
    }
    if (
      channel.channel_type !== "weixin" &&
      channel.channel_type !== "qqbot" &&
      !chatId.trim()
    ) {
      setError(t("chatIdRequired"))
      return
    }
    if (
      (channel.channel_type === "lark" || channel.channel_type === "qqbot") &&
      !appId.trim()
    ) {
      setError(t("appIdRequired"))
      return
    }
    const qqbotAppIdChanged =
      channel.channel_type === "qqbot" &&
      configuredAppId !== appId.trim() &&
      ownerOpenId.length > 0
    if (qqbotAppIdChanged && !resetOwnerBinding) {
      setError(t("qqbotResetRequired"))
      return
    }

    setLoading(true)
    setError(null)
    try {
      const normalizedAppId = appId.trim()
      const configJson =
        channel.channel_type === "weixin"
          ? JSON.stringify({ base_url: baseUrl })
          : channel.channel_type === "lark"
            ? JSON.stringify({ app_id: appId, chat_id: chatId })
            : channel.channel_type === "qqbot"
              ? JSON.stringify({
                  app_id: normalizedAppId,
                  ...(!resetOwnerBinding && ownerOpenId
                    ? { owner_openid: ownerOpenId }
                    : {}),
                })
              : JSON.stringify({ chat_id: chatId, topic_mode: topicMode })

      await updateChatChannel({
        id: channel.id,
        name: name.trim(),
        configJson,
        dailyReportEnabled,
        dailyReportTime: dailyReportEnabled ? dailyReportTime : null,
      })

      if (token.trim()) {
        await saveChatChannelToken(channel.id, token.trim())
      }

      handleOpenChange(false)
      onChannelUpdated()
      toast.success(t("editSuccess"))
    } catch (err: unknown) {
      const msg = toErrorMessage(err)
      setError(msg)
    } finally {
      setLoading(false)
    }
  }, [
    name,
    token,
    chatId,
    channel,
    ownerOpenId,
    configuredAppId,
    resetOwnerBinding,
    appId,
    baseUrl,
    topicMode,
    dailyReportEnabled,
    dailyReportTime,
    handleOpenChange,
    onChannelUpdated,
    t,
  ])

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{t("editChannel")}</DialogTitle>
        </DialogHeader>

        <div className="space-y-4">
          <div className="space-y-1.5">
            <label className="text-xs font-medium">{t("channelName")}</label>
            <Input
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={t("channelNamePlaceholder")}
            />
          </div>

          {(channel.channel_type === "lark" ||
            channel.channel_type === "qqbot") && (
            <div className="space-y-1.5">
              <label className="text-xs font-medium">App ID</label>
              <Input
                value={appId}
                onChange={(e) => setAppId(e.target.value)}
                placeholder={
                  channel.channel_type === "qqbot" ? "123456789" : "cli_xxxxx"
                }
              />
            </div>
          )}

          {channel.channel_type !== "weixin" && (
            <div className="space-y-1.5">
              <label className="text-xs font-medium">
                {channel.channel_type === "telegram"
                  ? "Bot Token"
                  : "App Secret"}
              </label>
              <Input
                type="password"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder={
                  hasToken ? t("tokenPlaceholderKeep") : t("tokenRequired")
                }
              />
            </div>
          )}

          {channel.channel_type !== "weixin" &&
            channel.channel_type !== "qqbot" && (
              <div className="space-y-1.5">
                <label className="text-xs font-medium">Chat ID</label>
                <Input
                  value={chatId}
                  onChange={(e) => setChatId(e.target.value)}
                  placeholder={
                    channel.channel_type === "telegram"
                      ? "-100123456789"
                      : "oc_xxxxx"
                  }
                />
              </div>
            )}

          {channel.channel_type === "telegram" && (
            <div className="rounded-md border border-border/70 p-3">
              <div className="flex items-center justify-between gap-3">
                <div>
                  <label htmlFor={topicModeId} className="text-xs font-medium">
                    {t("topicMode")}
                  </label>
                  <p className="mt-1 text-xs text-muted-foreground">
                    {t("topicModeHint")}
                  </p>
                </div>
                <Switch
                  id={topicModeId}
                  checked={topicMode}
                  onCheckedChange={setTopicMode}
                />
              </div>
            </div>
          )}

          {channel.channel_type === "weixin" && baseUrl && (
            <div className="space-y-1.5">
              <label className="text-xs font-medium">Base URL</label>
              <Input value={baseUrl} disabled />
            </div>
          )}

          {channel.channel_type === "qqbot" && (
            <div className="rounded-md border border-border/70 p-3 space-y-2">
              <p className="text-xs text-muted-foreground">
                {ownerOpenId && !resetOwnerBinding
                  ? t("qqbotOwnerBound")
                  : t("qqbotOwnerUnbound")}
              </p>
              {ownerOpenId && !resetOwnerBinding && (
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => setResetOwnerBindingChannelId(channel.id)}
                >
                  {t("qqbotResetOwner")}
                </Button>
              )}
              {resetOwnerBinding && (
                <p className="text-xs text-muted-foreground">
                  {t("qqbotResetOwnerHint")}
                </p>
              )}
            </div>
          )}

          <div className="flex items-center justify-between">
            <label className="text-xs font-medium">{t("dailyReport")}</label>
            <Switch
              checked={dailyReportEnabled}
              onCheckedChange={setDailyReportEnabled}
            />
          </div>

          {dailyReportEnabled && (
            <div className="space-y-1.5">
              <label className="text-xs font-medium">
                {t("dailyReportTime")}
              </label>
              <Input
                type="time"
                value={dailyReportTime}
                onChange={(e) => setDailyReportTime(e.target.value)}
              />
            </div>
          )}

          {error && (
            <div className="rounded-md border border-red-500/30 bg-red-500/5 px-3 py-2 text-xs text-red-400">
              {error}
            </div>
          )}
        </div>

        <DialogFooter>
          <Button
            variant="outline"
            onClick={() => handleOpenChange(false)}
            disabled={loading}
          >
            {t("cancel")}
          </Button>
          <Button onClick={handleSubmit} disabled={loading}>
            {loading && <Loader2 className="h-3.5 w-3.5 animate-spin mr-1" />}
            {t("save")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
