import type {
  AuditPage,
  Board,
  BoardRole,
  Command,
  CommandResult,
  Device,
  Health,
  IssuedDevice,
  Member,
  PendingCommand,
  Session,
  Snapshot,
  Workspace,
} from "./types.js";

const serverCodes = [
  "UNAUTHENTICATED",
  "PERMISSION_DENIED",
  "NOT_FOUND",
  "INVALID_ARGUMENT",
  "CONFLICT",
  "IDEMPOTENCY_CONFLICT",
  "LAST_MANAGER",
  "INTERNAL_ERROR",
] as const;

export type EnterpriseErrorCode =
  | (typeof serverCodes)[number]
  | "NETWORK_ERROR"
  | "SESSION_ENDED";

export class EnterpriseError extends Error {
  constructor(
    public readonly code: EnterpriseErrorCode,
    public readonly outcomeUnknown = false,
  ) {
    super(code);
    this.name = "EnterpriseError";
  }
}

export function asEnterpriseError(error: unknown): EnterpriseError {
  return error instanceof EnterpriseError
    ? error
    : new EnterpriseError("INTERNAL_ERROR", true);
}

function parseCode(body: unknown): EnterpriseErrorCode {
  if (typeof body === "object" && body !== null && "code" in body) {
    const code = body.code;
    if (serverCodes.some((candidate) => candidate === code)) {
      return code as (typeof serverCodes)[number];
    }
  }
  return "INTERNAL_ERROR";
}

/** A retry owns an immutable payload and ID, independent of editable form state. */
export function prepareCommand(
  boardId: string,
  command: Command,
  operationId: string = crypto.randomUUID(),
): PendingCommand {
  return Object.freeze({
    boardId,
    request: Object.freeze({
      operation_id: operationId,
      command: Object.freeze({ ...command }),
    }),
  });
}

/** Development transport only. Credentials never enter URLs or browser storage. */
export class EnterpriseClient {
  private token: string;
  private readonly lifetime = new AbortController();

  constructor(
    token: string,
    private readonly onAccessDenied: (code: EnterpriseErrorCode) => void,
    private readonly fetcher: typeof fetch = fetch,
    private readonly timeoutMs = 15_000,
  ) {
    this.token = token.trim();
  }

  dispose() {
    this.token = "";
    this.lifetime.abort();
  }

  private async request<T>(
    path: string,
    method = "GET",
    body?: unknown,
    authenticated = true,
  ): Promise<T> {
    if (this.lifetime.signal.aborted) {
      throw new EnterpriseError("SESSION_ENDED");
    }
    const controller = new AbortController();
    const stop = () => controller.abort();
    this.lifetime.signal.addEventListener("abort", stop, { once: true });
    const timeout = setTimeout(stop, this.timeoutMs);
    try {
      const headers: Record<string, string> = { Accept: "application/json" };
      if (authenticated) {
        headers.Authorization = `Bearer ${this.token}`;
      }
      if (body !== undefined) {
        headers["Content-Type"] = "application/json";
      }
      const response = await this.fetcher(`/enterprise-api${path}`, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        credentials: "omit",
        cache: "no-store",
        redirect: "error",
        signal: controller.signal,
      });
      // Revocation wins even when another response was already in flight.
      if (this.lifetime.signal.aborted) {
        throw new EnterpriseError("SESSION_ENDED");
      }
      if (response.status === 401 || response.status === 403) {
        const code =
          response.status === 401 ? "UNAUTHENTICATED" : "PERMISSION_DENIED";
        this.dispose();
        this.onAccessDenied(code);
        throw new EnterpriseError(code);
      }
      const data: unknown = await response.json().catch(() => null);
      if (this.lifetime.signal.aborted) {
        throw new EnterpriseError("SESSION_ENDED");
      }
      if (!response.ok) {
        throw new EnterpriseError(parseCode(data), response.status >= 500);
      }
      if (data === null || typeof data !== "object") {
        throw new EnterpriseError("INTERNAL_ERROR", method !== "GET");
      }
      return data as T;
    } catch (error) {
      if (error instanceof EnterpriseError) {
        throw error;
      }
      if (this.lifetime.signal.aborted) {
        throw new EnterpriseError("SESSION_ENDED");
      }
      throw new EnterpriseError("NETWORK_ERROR", method !== "GET");
    } finally {
      clearTimeout(timeout);
      this.lifetime.signal.removeEventListener("abort", stop);
    }
  }

  health() {
    return this.request<Health>("/health", "GET", undefined, false);
  }

  me() {
    return this.request<Session>("/v1/me");
  }

  workspaces() {
    return this.request<{ workspaces: Workspace[] }>("/v1/workspaces");
  }

  createWorkspace(name: string) {
    return this.request<Workspace>("/v1/workspaces", "POST", { name });
  }

  boards(workspaceId: string) {
    return this.request<{ boards: Board[] }>(
      `/v1/boards?workspace_id=${encodeURIComponent(workspaceId)}`,
    );
  }

  createBoard(workspaceId: string, name: string) {
    return this.request<Board>("/v1/boards", "POST", {
      workspace_id: workspaceId,
      name,
    });
  }

  members() {
    return this.request<{ members: Member[] }>("/v1/members");
  }

  createMember(name: string, role: "admin" | "member", deviceName: string) {
    return this.request<IssuedDevice & { member: Member }>(
      "/v1/members",
      "POST",
      { name, role, device_name: deviceName },
    );
  }

  setMemberStatus(id: string, status: "active" | "suspended") {
    return this.request<Member>(
      `/v1/members/${encodeURIComponent(id)}`,
      "PATCH",
      { status },
    );
  }

  devices() {
    return this.request<{ devices: Device[] }>("/v1/devices");
  }

  createDevice(memberId: string, name: string) {
    return this.request<IssuedDevice>(
      `/v1/members/${encodeURIComponent(memberId)}/devices`,
      "POST",
      { name },
    );
  }

  revokeDevice(id: string) {
    return this.request<{ revoked: true }>(
      `/v1/devices/${encodeURIComponent(id)}`,
      "DELETE",
    );
  }

  setBoardAccess(boardId: string, memberId: string, role: BoardRole | null) {
    return this.request<{ updated: true }>(
      `/v1/boards/${encodeURIComponent(boardId)}/access`,
      "PUT",
      { member_id: memberId, role },
    );
  }

  snapshot(boardId: string) {
    return this.request<Snapshot>(
      `/v1/boards/${encodeURIComponent(boardId)}/snapshot`,
    );
  }

  submit(pending: PendingCommand) {
    return this.request<CommandResult>(
      `/v1/boards/${encodeURIComponent(pending.boardId)}/commands`,
      "POST",
      pending.request,
    );
  }

  audit(after = 0) {
    return this.request<AuditPage>(`/v1/audit?after=${after}&limit=50`);
  }
}
