import type { ReactNode } from "react"
import { render, screen, waitFor } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { NextIntlClientProvider } from "next-intl"
import { beforeEach, describe, expect, it, vi } from "vitest"
import enMessages from "@/i18n/messages/en.json"
import type {
  AgentOptionsSnapshot,
  PromptInputBlock,
  WorkTask,
  WorkTaskDraft,
  WorkTaskFolderSettings,
} from "@/lib/types"

const headMock = vi.fn()

const testState = vi.hoisted(() => ({
  branches: vi.fn(),
  editorText: "Task description",
  editorBlocks: [] as PromptInputBlock[],
  settingsEffective: vi.fn(),
  templates: vi.fn(),
  submit: vi.fn(),
}))

const SNAPSHOT: AgentOptionsSnapshot = {
  modes: null,
  available_commands: [],
  config_options: [],
}

vi.mock("@/lib/api", () => ({
  gitListAllBranches: (...args: unknown[]) => testState.branches(...args),
  getGitBranch: (...args: unknown[]) => headMock(...args),
  workTaskSettingsEffective: (...args: unknown[]) =>
    testState.settingsEffective(...args),
  workTaskTemplateList: (...args: unknown[]) => testState.templates(...args),
  workTaskTemplateSave: vi.fn(),
  workTaskTemplateDelete: vi.fn(),
}))

vi.mock("@/stores/app-workspace-store", () => {
  const state = {
    folders: [
      {
        id: 1,
        name: "proj",
        alias: null,
        parent_id: null,
        kind: "regular",
        path: "/tmp/proj",
        default_agent_type: null,
      },
    ],
  }
  const useStore = (selector: (s: typeof state) => unknown) => selector(state)
  useStore.getState = () => state
  return { useAppWorkspaceStore: useStore }
})

vi.mock("@/components/chat/agent-selector", () => ({
  AgentSelector: (props: {
    defaultAgentType?: string
    onSelect: (agent: string) => void
  }) => (
    <div>
      <span data-testid="selected-agent">{props.defaultAgentType}</span>
      <button type="button" onClick={() => props.onSelect("claude_code")}>
        choose claude
      </button>
    </div>
  ),
}))

vi.mock("@/components/automations/agent-config-section", () => ({
  AgentConfigSection: (props: {
    modeId: string | null
    configValues: Record<string, string>
  }) => (
    <div
      data-testid="agent-config"
      data-mode-id={props.modeId ?? ""}
      data-config-values={JSON.stringify(props.configValues)}
    />
  ),
  effectiveSelections: (
    _snapshot: AgentOptionsSnapshot | null,
    modeId: string | null,
    configValues: Record<string, string>
  ) => ({ mode_id: modeId, config_values: configValues }),
  snapshotLabels: () => ({}),
  useBooleanOptionLabels: () => ({ on: "On", off: "Off" }),
}))

vi.mock("@/components/automations/use-agent-options", () => ({
  useAgentOptions: () => ({
    snapshot: SNAPSHOT,
    snapshotAgentType: "claude_code",
    loading: false,
    error: null,
    reload: vi.fn(),
    ensure: () => Promise.resolve(SNAPSHOT),
  }),
}))

// The real composer is a Tiptap editor. This merged stub keeps the editable
// textarea used by the base-branch tests and exposes the richer prompt blocks
// needed by the task-setting inheritance tests.
vi.mock("./task-message-composer", async () => {
  const { forwardRef, useImperativeHandle, useState } = await import("react")
  type StubProps = {
    defaultText?: string
    ariaLabel?: string
    onChange?: (text: string) => void
    bottomBarExtra?: ReactNode
  }
  return {
    TaskMessageComposer: forwardRef(function Stub(
      props: StubProps,
      ref: React.Ref<unknown>
    ) {
      const [text, setText] = useState(props.defaultText ?? "")
      useImperativeHandle(
        ref,
        () => ({
          getText: () => testState.editorText,
          getPromptBlocks: () => testState.editorBlocks,
          hasAttachments: () =>
            testState.editorBlocks.some((block) => block.type !== "text"),
          hasUploadingImage: () => false,
          focus: () => {},
        }),
        []
      )
      return (
        <div data-testid="task-composer">
          <textarea
            aria-label={props.ariaLabel}
            value={text}
            onChange={(event) => {
              const next = event.target.value
              setText(next)
              testState.editorText = next
              testState.editorBlocks = [{ type: "text", text: next }]
              props.onChange?.(next)
            }}
          />
          {props.bottomBarExtra}
        </div>
      )
    }),
  }
})

vi.mock("@/hooks/use-scrollbar-safe-dismiss", () => ({
  useScrollbarSafeDismiss: () => ({
    contentRef: { current: null },
    onPointerDownOutside: vi.fn(),
    onFocusOutside: vi.fn(),
  }),
}))

import { TaskEditorDialog } from "./task-editor-dialog"

function settings(
  overrides: Partial<WorkTaskFolderSettings> = {}
): WorkTaskFolderSettings {
  return {
    default_agent_type: null,
    mode_id: null,
    config_values: {},
    auto_process: false,
    max_concurrent: 2,
    merge_strategy: "squash",
    auto_merge: false,
    delete_worktree_default: true,
    auto_compact_percent: 0,
    ...overrides,
  }
}

function ranTask(overrides?: Partial<WorkTask>): WorkTask {
  return {
    id: 7,
    folder_id: 1,
    title: "Polish the feature",
    config: {
      prompt_blocks: [{ type: "text", text: "do it" }],
      display_text: "do it",
      config_values: {},
    },
    status: "review",
    failure_reason: null,
    last_error: null,
    run_seq: 1,
    sort_order: 1,
    worktree_folder_id: 9,
    conversation_id: 3,
    connection_id: null,
    base_branch: "feature",
    base_sha: "abc",
    work_branch: "task/7",
    cleanup_state: null,
    verdict: null,
    result_summary: null,
    files_changed: 0,
    additions: 0,
    deletions: 0,
    merge_commit: null,
    preflight: null,
    archived_at: null,
    scheduled_at: null,
    created_at: "2026-08-01T00:00:00Z",
    updated_at: "2026-08-01T00:00:00Z",
    started_at: null,
    settled_at: null,
    finished_at: null,
    ...overrides,
  }
}

function taskRow(config: NonNullable<WorkTask["config"]>): WorkTask {
  return {
    id: 1,
    folder_id: 1,
    title: "Fix login",
    config,
    status: "todo",
    failure_reason: null,
    last_error: null,
    run_seq: 0,
    sort_order: 0,
    worktree_folder_id: null,
    agent_type: config.agent_type ?? null,
    conversation_id: null,
    connection_id: null,
    base_branch: null,
    base_sha: null,
    work_branch: null,
    cleanup_state: null,
    verdict: null,
    result_summary: null,
    files_changed: null,
    additions: null,
    deletions: null,
    merge_commit: null,
    preflight: null,
    archived_at: null,
    scheduled_at: null,
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
    started_at: null,
    settled_at: null,
    finished_at: null,
  }
}

function renderEditor(task: WorkTask | null = null) {
  const onSubmit = vi.fn<(draft: WorkTaskDraft) => Promise<void>>(() =>
    Promise.resolve()
  )
  render(
    <NextIntlClientProvider locale="en" messages={enMessages}>
      <TaskEditorDialog
        open
        onOpenChange={() => {}}
        task={task}
        defaultFolderId={1}
        onSubmit={onSubmit}
      />
    </NextIntlClientProvider>
  )
  return onSubmit
}

function renderInheritanceEditor(row: WorkTask) {
  return render(
    <NextIntlClientProvider locale="en" messages={enMessages}>
      <TaskEditorDialog
        open
        onOpenChange={() => {}}
        task={row}
        defaultFolderId={null}
        onSubmit={testState.submit}
      />
    </NextIntlClientProvider>
  )
}

async function fillBrief(user: ReturnType<typeof userEvent.setup>) {
  await user.type(screen.getByLabelText("Title"), "Polish the feature")
  await user.type(screen.getByLabelText("Task description"), "do it")
}

beforeEach(() => {
  testState.branches.mockReset().mockResolvedValue({
    local: ["main", "feature"],
    remote: ["origin/release"],
    worktree_branches: [],
    main_worktree_branch: null,
  })
  headMock.mockReset().mockResolvedValue("main")
  testState.editorText = "Task description"
  testState.editorBlocks = [{ type: "text", text: "Task description" }]
  testState.settingsEffective.mockReset().mockResolvedValue(settings())
  testState.templates.mockReset().mockResolvedValue([])
  testState.submit.mockReset().mockResolvedValue(undefined)
})

describe("TaskEditorDialog base branch", () => {
  it("saves the branch the task was created for", async () => {
    const user = userEvent.setup()
    const onSubmit = renderEditor()
    await fillBrief(user)

    await user.click(screen.getByRole("button", { name: "Base branch" }))
    await user.click(await screen.findByText("feature"))
    await user.click(screen.getByRole("button", { name: "Save" }))

    await waitFor(() => expect(onSubmit).toHaveBeenCalled())
    expect(onSubmit.mock.calls[0][0].config.base_branch).toBe("feature")
  })

  it("an unpicked base reads HEAD and the branch the project folder is on", async () => {
    const user = userEvent.setup()
    renderEditor()

    // Unpicked = the project folder's checkout when the task starts, so the
    // picker says HEAD and names the branch that checkout is on right now.
    const trigger = screen.getByRole("button", { name: "Base branch" })
    await waitFor(() => expect(trigger).toHaveTextContent(/^HEAD\s*main$/))
    expect(headMock).toHaveBeenCalledWith("/tmp/proj")

    await user.click(trigger)
    const entry = await screen.findByRole("option", { name: /^HEAD/ })
    expect(entry).toHaveTextContent(/^HEAD\s*main$/)
    expect(entry).toHaveAttribute(
      "title",
      "Current branch when the task starts"
    )
  })

  it("leaves the base unset when nothing is picked", async () => {
    const user = userEvent.setup()
    const onSubmit = renderEditor()
    await fillBrief(user)

    await user.click(screen.getByRole("button", { name: "Save" }))

    await waitFor(() => expect(onSubmit).toHaveBeenCalled())
    expect(onSubmit.mock.calls[0][0].config.base_branch).toBeNull()
  })

  it("switching project drops a branch picked for the previous one", async () => {
    const user = userEvent.setup()
    const onSubmit = renderEditor()
    await fillBrief(user)

    await user.click(screen.getByRole("button", { name: "Base branch" }))
    await user.click(await screen.findByText("feature"))
    await user.click(screen.getByRole("button", { name: "proj" }))
    await user.click(await screen.findByRole("option", { name: /proj/ }))
    await user.click(screen.getByRole("button", { name: "Save" }))

    await waitFor(() => expect(onSubmit).toHaveBeenCalled())
    expect(onSubmit.mock.calls[0][0].config.base_branch).toBeNull()
  })

  it("shows the recorded base of a task that already ran, read-only", async () => {
    renderEditor(ranTask())

    const trigger = await screen.findByRole("button", { name: "Base branch" })
    expect(trigger).toHaveTextContent("feature")
    expect(trigger).toBeDisabled()
    // Nor is the checkout's HEAD any of its business.
    expect(trigger).not.toHaveTextContent("HEAD")
    expect(headMock).not.toHaveBeenCalled()
  })

  it("saving a task that already ran keeps the request it was created with", async () => {
    const user = userEvent.setup()
    const onSubmit = renderEditor(ranTask({ status: "failed" }))

    await user.click(await screen.findByRole("button", { name: "Save" }))

    await waitFor(() => expect(onSubmit).toHaveBeenCalled())
    expect(onSubmit.mock.calls[0][0].config.base_branch).toBeNull()
  })

  it("keeps an explicit choice across an edit", async () => {
    const user = userEvent.setup()
    const onSubmit = renderEditor(
      ranTask({
        status: "failed",
        config: {
          prompt_blocks: [{ type: "text", text: "do it" }],
          display_text: "do it",
          config_values: {},
          base_branch: "feature",
        },
      })
    )

    await user.click(await screen.findByRole("button", { name: "Save" }))

    await waitFor(() => expect(onSubmit).toHaveBeenCalled())
    expect(onSubmit.mock.calls[0][0].config.base_branch).toBe("feature")
  })

  it("a task whose worktree was cleaned up does not inherit its old base", async () => {
    const user = userEvent.setup()
    const onSubmit = renderEditor(
      ranTask({ status: "failed", worktree_folder_id: null })
    )

    const trigger = await screen.findByRole("button", { name: "Base branch" })
    expect(trigger).toBeEnabled()
    await waitFor(() => expect(trigger).toHaveTextContent(/^HEAD\s*main$/))
    await user.click(screen.getByRole("button", { name: "Save" }))

    await waitFor(() => expect(onSubmit).toHaveBeenCalled())
    expect(onSubmit.mock.calls[0][0].config.base_branch).toBeNull()
  })

  it("offers no branch on a pull-request task, whose base is the review's", async () => {
    renderEditor(ranTask({ source_kind: "forge_pr" }))

    await screen.findByRole("button", { name: "proj" })
    expect(
      screen.queryByRole("button", { name: "Base branch" })
    ).not.toBeInTheDocument()
  })
})

describe("TaskEditorDialog task-setting inheritance", () => {
  it("shows matching effective settings for an explicit Codex agent", async () => {
    testState.settingsEffective.mockResolvedValue(
      settings({
        default_agent_type: "codex",
        mode_id: "global-mode",
        config_values: { model: "global-model" },
      })
    )
    renderInheritanceEditor(
      taskRow({
        display_text: "Task description",
        prompt_blocks: [{ type: "text", text: "Task description" }],
        agent_type: "codex",
        mode_id: null,
        config_values: {},
      })
    )

    await waitFor(() =>
      expect(screen.getByTestId("selected-agent")).toHaveTextContent("codex")
    )
    await waitFor(() => {
      expect(screen.getByTestId("agent-config")).toHaveAttribute(
        "data-mode-id",
        "global-mode"
      )
      expect(screen.getByTestId("agent-config")).toHaveAttribute(
        "data-config-values",
        JSON.stringify({ model: "global-model" })
      )
    })
  })

  it("does not apply Codex settings to a different explicit agent", async () => {
    testState.settingsEffective.mockResolvedValue(
      settings({
        default_agent_type: "codex",
        mode_id: "global-mode",
        config_values: { model: "global-model" },
      })
    )
    renderInheritanceEditor(
      taskRow({
        display_text: "Task description",
        prompt_blocks: [{ type: "text", text: "Task description" }],
        agent_type: "claude_code",
        mode_id: null,
        config_values: {},
      })
    )

    await waitFor(() => expect(testState.settingsEffective).toHaveBeenCalled())
    expect(screen.getByTestId("selected-agent")).toHaveTextContent(
      "claude_code"
    )
    expect(screen.getByTestId("agent-config")).toHaveAttribute(
      "data-mode-id",
      ""
    )
    expect(screen.getByTestId("agent-config")).toHaveAttribute(
      "data-config-values",
      "{}"
    )
  })

  it("saves the shown settings for an explicit Codex task", async () => {
    testState.settingsEffective.mockResolvedValue(
      settings({
        default_agent_type: "codex",
        mode_id: "global-mode",
        config_values: { model: "global-model" },
      })
    )
    const blocks: PromptInputBlock[] = [
      { type: "text", text: "updated description" },
      { type: "image", data: "aGk=", mime_type: "image/png", uri: null },
    ]
    testState.editorText = "updated description"
    testState.editorBlocks = blocks
    renderInheritanceEditor(
      taskRow({
        display_text: "Task description",
        prompt_blocks: [{ type: "text", text: "Task description" }],
        agent_type: "codex",
        mode_id: null,
        config_values: {},
      })
    )

    await waitFor(() =>
      expect(screen.getByTestId("agent-config")).toHaveAttribute(
        "data-mode-id",
        "global-mode"
      )
    )
    await userEvent.setup().click(screen.getByRole("button", { name: "Save" }))

    await waitFor(() => expect(testState.submit).toHaveBeenCalledTimes(1))
    const draft = testState.submit.mock.calls[0][0]
    expect(draft.config).toMatchObject({
      agent_type: "codex",
      mode_id: "global-mode",
      config_values: { model: "global-model" },
      display_text: "updated description",
      prompt_blocks: blocks,
    })
  })

  it("saves empty inherited fields after reset", async () => {
    testState.settingsEffective.mockResolvedValue(
      settings({
        default_agent_type: "codex",
        mode_id: "global-mode",
        config_values: { model: "global-model" },
      })
    )
    renderInheritanceEditor(
      taskRow({
        display_text: "Task description",
        prompt_blocks: [{ type: "text", text: "Task description" }],
        agent_type: "codex",
        mode_id: null,
        config_values: {},
      })
    )

    await waitFor(() =>
      expect(screen.getByTestId("agent-config")).toHaveAttribute(
        "data-mode-id",
        "global-mode"
      )
    )
    const user = userEvent.setup()
    await user.click(screen.getByRole("button", { name: "Reset to inherited" }))
    await user.click(screen.getByRole("button", { name: "Save" }))

    await waitFor(() => expect(testState.submit).toHaveBeenCalledTimes(1))
    expect(testState.submit.mock.calls[0][0].config).toMatchObject({
      agent_type: null,
      mode_id: null,
      config_values: {},
    })
  })
})
