/** The Codex card exposes its configuration as read-only downstream state. */
import { render, screen, within } from "@testing-library/react"
import { NextIntlClientProvider } from "next-intl"
import { beforeEach, describe, expect, it, vi } from "vitest"

import enMessages from "@/i18n/messages/en.json"
import {
  acpListAgents,
  acpUpdateAgentConfig,
  acpUpdateAgentEnv,
  codexBundledCatalog,
  listModelProviders,
} from "@/lib/api"
import type { AcpAgentInfo, CodexModelInfo } from "@/lib/types"

import { AcpAgentSettings } from "./acp-agent-settings"

vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<Record<string, unknown>>()
  // Every function is stubbed so nothing here can reach the real transport;
  // the calls these tests don't need simply never settle.
  return Object.fromEntries(
    Object.entries(actual).map(([name, value]) => [
      name,
      typeof value === "function" ? vi.fn(() => new Promise(() => {})) : value,
    ])
  )
})
vi.mock("next/navigation", () => {
  const params = new URLSearchParams()
  return { useSearchParams: () => params }
})
vi.mock("sonner", () => ({
  toast: Object.assign(vi.fn(), {
    success: vi.fn(),
    error: vi.fn(),
    info: vi.fn(),
    warning: vi.fn(),
  }),
}))

const API_KEY_AUTH = JSON.stringify({ OPENAI_API_KEY: "sk-test" })

const CONFIG = [
  "# mine",
  'model = "gpt-5.5"',
  'model_provider = "codeg"',
  "",
  "[model_providers.codeg]",
  'base_url = "https://relay.example/v1"',
  'name = "codeg"',
  'wire_api = "responses"',
  "requires_openai_auth = true",
  "",
].join("\n")

const OFFICIALS: CodexModelInfo[] = [
  { slug: "gpt-5.5", display_name: "GPT-5.5", visibility: "list" },
  { slug: "gpt-5.5-mini", display_name: "GPT-5.5 mini", visibility: "list" },
]

function codexAgent(overrides: Partial<AcpAgentInfo> = {}): AcpAgentInfo {
  return {
    agent_type: "codex",
    skills_capable: true,
    registry_id: "codex-acp",
    registry_version: "2.2.2",
    supports_custom_version: false,
    name: "Codex CLI",
    description: "",
    available: true,
    distribution_type: "npx",
    is_acp_adapter: true,
    custom_source: null,
    enabled: true,
    sort_order: 0,
    installed_version: "2.2.2",
    host_tools_agent_mode: false,
    env: {},
    config_json: null,
    config_file_path: null,
    codex_auth_json: API_KEY_AUTH,
    cline_secrets_json: null,
    codex_config_toml: CONFIG,
    codex_model_catalog: null,
    codex_sandbox_settings: null,
    grok_config_toml: null,
    grok_settings: null,
    hermes_config_yaml: null,
    cursor_cli_config_json: null,
    cursor_settings: null,
    model_provider_id: null,
    icon_url: null,
    ...overrides,
  }
}

async function openPanel(agent: AcpAgentInfo) {
  vi.mocked(acpListAgents).mockResolvedValue([agent])
  render(
    <NextIntlClientProvider locale="en" messages={enMessages}>
      <AcpAgentSettings />
    </NextIntlClientProvider>
  )
  await screen.findByText("config.toml (native)", { selector: "label" })
}

function expectCodexWritesBlocked() {
  expect(
    screen.getByRole("button", { name: "Save Codex Config" })
  ).toBeDisabled()
  expect(acpUpdateAgentConfig).not.toHaveBeenCalled()
  expect(acpUpdateAgentEnv).not.toHaveBeenCalled()
}

describe("AcpAgentSettings — Codex configuration is read-only", () => {
  beforeEach(() => {
    vi.clearAllMocks()
    vi.mocked(listModelProviders).mockResolvedValue([])
    vi.mocked(acpUpdateAgentEnv).mockResolvedValue(0)
    vi.mocked(acpUpdateAgentConfig).mockResolvedValue(0)
    vi.mocked(codexBundledCatalog).mockResolvedValue(OFFICIALS)
  })

  it("shows the official model list without allowing removals", async () => {
    await openPanel(codexAgent())
    const row = (await screen.findByText("GPT-5.5 mini")).closest(
      "[data-model-row]"
    ) as HTMLElement
    expect(
      within(row).getByRole("button", { name: "Set as default" })
    ).toBeDisabled()
    expect(within(row).queryByRole("button", { name: "Remove" })).toBeNull()
    expectCodexWritesBlocked()
  })

  it("keeps the configured default model visible without allowing changes", async () => {
    await openPanel(codexAgent())
    const row = (await screen.findByText("GPT-5.5")).closest(
      "[data-model-row]"
    ) as HTMLElement
    expect(screen.getByText("GPT-5.5 mini")).toBeInTheDocument()
    expect(
      within(row).getByRole("button", { name: "Default model" })
    ).toBeDisabled()
    expectCodexWritesBlocked()
  })

  it("shows a customized model entry without allowing edits", async () => {
    await openPanel(
      codexAgent({
        codex_model_catalog: JSON.stringify({
          customs: [{ slug: "custom-model", base: "gpt-5.5" }],
        }),
      })
    )
    const customSlug = screen.getByDisplayValue("custom-model")
    expect(customSlug).toHaveAttribute("readonly")
    for (const button of screen.getAllByRole("button", {
      name: "Set as default",
    })) {
      expect(button).toBeDisabled()
    }
    expect(screen.queryByRole("button", { name: "Remove" })).toBeNull()
    expectCodexWritesBlocked()
  })

  it("shows config.toml without allowing edits or saves", async () => {
    await openPanel(codexAgent())
    const label = screen.getByText("config.toml (native)", {
      selector: "label",
    })
    const editor = within(label.parentElement as HTMLElement).getByRole(
      "textbox"
    )
    expect(editor).toHaveValue(CONFIG)
    expect(editor).toHaveAttribute("readonly")
    expectCodexWritesBlocked()
  })

  it("shows Auth Mode without allowing authentication changes", async () => {
    await openPanel(
      codexAgent({
        codex_model_catalog: '{"customs":[],"catalog":{"format":"native"}}',
      })
    )
    const label = screen.getByText("Auth Mode", { selector: "label" })
    expect(
      within(label.parentElement as HTMLElement).getByRole("combobox")
    ).toBeDisabled()
    expectCodexWritesBlocked()
  })

  it("does not expose a Codex save error path while saving is disabled", async () => {
    vi.mocked(acpUpdateAgentConfig).mockRejectedValue(
      new Error(
        'no model is left\n[codexCatalog] {"code":"no_usable_models","params":{}}'
      )
    )
    await openPanel(codexAgent())
    expectCodexWritesBlocked()
  })
})
