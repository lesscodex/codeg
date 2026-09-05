import {afterEach, describe, expect, it, vi} from "vitest";
import {createCodexMockTestFixture, createTestModel} from "../acp-test-utils";
import type {McpStartupResult} from "../../mcp/McpStartupTracker";

describe("ACP session fork", () => {
    afterEach(() => {
        vi.restoreAllMocks();
    });

    it("creates and installs a forked session", async () => {
        const fixture = createCodexMockTestFixture();
        const agent = fixture.getCodexAcpAgent();
        const client = fixture.getCodexAcpClient();
        const model = createTestModel({id: "gpt-5"});

        vi.spyOn(client, "readAuthRequirement").mockResolvedValue({required: false, account: null});
        vi.spyOn(client, "getAccount").mockResolvedValue({account: null, requiresOpenaiAuth: false});
        vi.spyOn(client, "listSkills").mockResolvedValue({data: []});
        const forkSpy = vi.spyOn(client, "forkSession").mockResolvedValue({
            sessionId: "fork-id",
            currentModelId: "gpt-5[medium]",
            models: [model],
            collaborationMode: "default",
            modelProvider: "openai",
            currentServiceTier: null,
            additionalDirectories: [],
        });

        const response = await agent.forkSession({
            sessionId: "source-id",
            cwd: "/workspace",
            mcpServers: [],
        });

        expect(response.sessionId).toBe("fork-id");
        expect(agent.getSessionState("fork-id").cwd).toBe("/workspace");
        // Forking creates a session, so the connection reports the account it was
        // created under (`authStatus` extension). Nothing else is sent.
        expect(fixture.getAcpConnectionEvents([])).toEqual([
            {
                method: "notify",
                args: ["_auth/status_update", {authStatus: {kind: "none", label: "Not logged in"}}],
            },
        ]);
        expect(forkSpy).toHaveBeenCalledWith({
            sessionId: "source-id",
            cwd: "/workspace",
            mcpServers: [],
        }, expect.any(Function));
    });

    it("waits for MCP startup before completing session fork", async () => {
        const fixture = createCodexMockTestFixture();
        const agent = fixture.getCodexAcpAgent();
        const client = fixture.getCodexAcpClient();
        const mcpStartup = deferred<McpStartupResult>();

        vi.spyOn(client, "readAuthRequirement").mockResolvedValue({required: false, account: null});
        vi.spyOn(client, "getAccount").mockResolvedValue({account: null, requiresOpenaiAuth: false});
        vi.spyOn(client, "listSkills").mockResolvedValue({data: []});
        vi.spyOn(client, "forkSession").mockResolvedValue({
            sessionId: "fork-id",
            currentModelId: "gpt-5[medium]",
            models: [createTestModel({id: "gpt-5"})],
            collaborationMode: "default",
            currentServiceTier: null,
            additionalDirectories: [],
        });
        const awaitMcpStartupSpy = vi.spyOn(client, "awaitMcpServerStartup")
            .mockReturnValue(mcpStartup.promise);

        const forkPromise = agent.forkSession({
            sessionId: "source-id",
            cwd: "/workspace",
            mcpServers: [{name: "fork-mcp", command: "npx", args: ["fork"], env: []}],
            _meta: {mcpStartupAwaitTimeoutMs: 30_000},
        });
        let forkSettled = false;
        void forkPromise.then(
            () => { forkSettled = true; },
            () => { forkSettled = true; },
        );

        await vi.waitFor(() => {
            expect(awaitMcpStartupSpy).toHaveBeenCalledWith(["fork-mcp"], expect.any(Number), {threadId: "fork-id", signal: expect.any(AbortSignal)});
        });
        expect(forkSettled).toBe(false);

        mcpStartup.resolve({ready: ["fork-mcp"], failed: [], cancelled: []});
        await expect(forkPromise).resolves.toMatchObject({sessionId: "fork-id", modes: expect.any(Object)});
    });

    it("leaves no MCP startup wait after a fork without the startup wait", async () => {
        const fixture = createCodexMockTestFixture();
        const agent = fixture.getCodexAcpAgent();
        const client = fixture.getCodexAcpClient();

        vi.spyOn(client, "readAuthRequirement").mockResolvedValue({required: false, account: null});
        vi.spyOn(client, "getAccount").mockResolvedValue({account: null, requiresOpenaiAuth: false});
        vi.spyOn(client, "listSkills").mockResolvedValue({data: []});
        vi.spyOn(client, "forkSession").mockResolvedValue({
            sessionId: "fork-id",
            currentModelId: "gpt-5[medium]",
            models: [createTestModel({id: "gpt-5"})],
            collaborationMode: "default",
            currentServiceTier: null,
            additionalDirectories: [],
        });

        await agent.forkSession({
            sessionId: "source-id",
            cwd: "/workspace",
            mcpServers: [{name: "fork-mcp", command: "npx", args: ["fork"], env: []}],
        });

        const tracker = client.appServerClient.mcpStartup as unknown as {waiters: unknown[]};
        expect(tracker.waiters).toHaveLength(0);
    });

    it("accepts lastTurnId on the standard session/fork request", async () => {
        const fixture = createCodexMockTestFixture();
        const agent = fixture.getCodexAcpAgent();
        const client = fixture.getCodexAcpClient();
        const model = createTestModel({id: "gpt-5"});

        vi.spyOn(client, "readAuthRequirement").mockResolvedValue({required: false, account: null});
        vi.spyOn(client, "getAccount").mockResolvedValue({account: null, requiresOpenaiAuth: false});
        vi.spyOn(client, "listSkills").mockResolvedValue({data: []});
        const forkSpy = vi.spyOn(client, "forkSession").mockResolvedValue({
            sessionId: "fork-id",
            currentModelId: "gpt-5[medium]",
            models: [model],
            collaborationMode: "default",
            modelProvider: "openai",
            currentServiceTier: null,
            additionalDirectories: [],
        });
        const response = await agent.forkSession({
            sessionId: "source-id",
            cwd: "/workspace",
            lastTurnId: "completed-turn-id",
            _meta: {
                jetbrains: {air: {fork: {
                    version: 1,
                    messageId: "turn-7",
                    messageFingerprint: "sha256:41153d2b46c2869f4021958d44dac18888247fd999507c28970be299a8de4a0f",
                    messageOccurrence: 1,
                }}},
            },
        });

        expect(response.sessionId).toBe("fork-id");
        expect(forkSpy).toHaveBeenCalledWith(expect.objectContaining({
            sessionId: "source-id",
            cwd: "/workspace",
            lastTurnId: "completed-turn-id",
        }));
    });
});

function deferred<T>(): {promise: Promise<T>, resolve: (value: T) => void} {
    let resolve: (value: T) => void = () => {};
    const promise = new Promise<T>((innerResolve) => {
        resolve = innerResolve;
    });
    return {promise, resolve};
}
