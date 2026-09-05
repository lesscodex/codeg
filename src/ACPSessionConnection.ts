import * as acp from "@agentclientprotocol/sdk";
import {
    type AcpSessionUpdate,
    asSdkSessionNotification,
} from "./AcpSessionExtensions";
import {CODEX_TURN_STARTED_METHOD} from "./AcpExtensions";

export type AcpClientConnection = Pick<acp.AgentContext, "notify" | "request">;

export class ACPSessionConnection {
    private readonly connection: AcpClientConnection;
    readonly sessionId: string;

    constructor(connection: AcpClientConnection, sessionId: string) {
        this.connection = connection;
        this.sessionId = sessionId;
    }

    async update(update: UpdateSessionEvent, sessionId: string = this.sessionId) {
        await this.connection.notify(acp.methods.client.session.update, asSdkSessionNotification({
            sessionId,
            update: update
        }));
    }

    async turnStarted(turnId: string): Promise<void> {
        await this.connection.notify(CODEX_TURN_STARTED_METHOD, {
            sessionId: this.sessionId,
            turnId,
        });
    }
}

export type UpdateSessionEvent = AcpSessionUpdate;
