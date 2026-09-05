import {describe, expect, it, vi} from "vitest";
import {CODEX_TURN_STARTED_METHOD} from "../../AcpExtensions";
import {setupPromptTestSession} from "../acp-test-utils";

describe("Codex turn-start handoff", () => {
    it("forwards the app-server turn id through the callback and Codex extension", async () => {
        const sessionId = "test-session-id";
        const {mockFixture} = setupPromptTestSession({sessionId});
        const onTurnStarted = vi.fn();

        await mockFixture.getCodexAcpAgent().prompt(
            {
                sessionId,
                prompt: [{type: "text", text: "test prompt"}],
            },
            undefined,
            onTurnStarted,
        );

        await vi.waitFor(() => {
            expect(onTurnStarted).toHaveBeenCalledWith("turn-id");
            expect(mockFixture.getAcpConnectionEvents([])).toContainEqual({
                method: "notify",
                args: [
                    CODEX_TURN_STARTED_METHOD,
                    {sessionId, turnId: "turn-id"},
                ],
            });
        });
    });
});
