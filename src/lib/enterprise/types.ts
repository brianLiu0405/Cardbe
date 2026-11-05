export type OrganizationRole = "owner" | "admin" | "member";
export type BoardRole = "manager" | "editor" | "viewer";

export interface Member {
  id: string;
  name: string;
  role: OrganizationRole;
  status: "active" | "suspended";
}

export interface Device {
  id: string;
  name: string;
  user_id: string;
  revoked: boolean;
}

export interface Session {
  organization: { id: string; name: string };
  user: Member;
  device: { id: string; name: string; revoked: boolean };
}

export interface Health {
  status: "ok";
  mode: "prototype";
  storage: "sqlite";
  version: string;
}

export interface Workspace {
  id: string;
  name: string;
}

export interface Board {
  id: string;
  workspace_id: string;
  name: string;
  role: BoardRole;
  revision: number;
}

export interface Task {
  id: string;
  title: string;
  description: string;
  status: "todo" | "done";
  assignee_id: string | null;
  version: number;
}

export interface Comment {
  id: string;
  task_id: string;
  author_id: string;
  body: string;
  created_at: string;
}

export interface Snapshot {
  board: Board;
  tasks: Task[];
  comments: Comment[];
}

export type Command =
  | {
      type: "task.create";
      title: string;
      description?: string;
      assignee_id?: string | null;
    }
  | {
      type: "task.update";
      task_id: string;
      base_version: number;
      title?: string;
      description?: string;
      status?: "todo" | "done";
      assignee_id?: string | null;
    }
  | { type: "comment.create"; task_id: string; body: string };

export interface CommandRequest {
  operation_id: string;
  command: Command;
}

export interface CommandResult {
  operation_id: string;
  revision: number;
  replayed: boolean;
  result: { task?: Task; comment?: Comment };
}

export interface PendingCommand {
  readonly boardId: string;
  readonly request: Readonly<CommandRequest>;
}

export interface IssuedDevice {
  device: Device;
  token: string;
}

export interface AuditEvent {
  id: number;
  actor_id: string;
  device_id: string;
  action: string;
  resource_id: string;
  outcome: string;
  created_at: string;
  operation_id: string | null;
}

export interface AuditPage {
  events: AuditEvent[];
  next_cursor: number;
}
