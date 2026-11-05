<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { m } from "#lib/paraglide/messages.js";
  import { Button } from "#lib/components/ui/button/index.js";
  import { Input } from "#lib/components/ui/input/index.js";
  import { Label } from "#lib/components/ui/label/index.js";
  import * as Card from "#lib/components/ui/card/index.js";
  import * as Tabs from "#lib/components/ui/tabs/index.js";
  import { ScrollArea } from "#lib/components/ui/scroll-area/index.js";
  import {
    EnterpriseClient,
    EnterpriseError,
    asEnterpriseError,
  } from "#lib/enterprise/client.js";
  import type { Board, Session, Workspace } from "#lib/enterprise/types.js";
  import FieldSelect from "./field-select.svelte";
  import ErrorNotice from "./error-notice.svelte";
  import BoardPanel from "./board-panel.svelte";
  import MembersPanel from "./members-panel.svelte";
  import AccessPanel from "./access-panel.svelte";
  import AuditPanel from "./audit-panel.svelte";

  let { client, session }: { client: EnterpriseClient; session: Session } =
    $props();
  const canAdminister = $derived(session.user.role !== "member");
  let workspaces = $state<Workspace[]>([]);
  let boards = $state<Board[]>([]);
  let workspaceId = $state("");
  let boardId = $state("");
  let workspaceName = $state("");
  let boardName = $state("");
  let busy = $state(false);
  let uncertain = $state(false);
  let commandPending = $state(false);
  let error = $state<EnterpriseError | null>(null);
  let tab = $state("board");
  let boardEpoch = 0;
  let disposed = false;
  const selectedBoard = $derived(boards.find((board) => board.id === boardId));

  function removeUnavailableBoard(id: string) {
    boardEpoch++;
    boards = boards.filter((board) => board.id !== id);
    if (boardId === id) {
      boardId = "";
      commandPending = false;
    }
    error = new EnterpriseError("NOT_FOUND");
  }

  async function loadBoards(prefer = "") {
    const generation = boardEpoch;
    const workspace = workspaceId;
    const next = workspace ? (await client.boards(workspace)).boards : [];
    if (disposed || generation !== boardEpoch || workspace !== workspaceId) {
      return;
    }
    boards = next;
    boardId = next.some((board) => board.id === prefer)
      ? prefer
      : (next[0]?.id ?? "");
  }

  async function refresh() {
    if (busy || commandPending) {
      return;
    }
    busy = true;
    error = null;
    const generation = boardEpoch;
    try {
      const result = await client.workspaces();
      if (disposed || generation !== boardEpoch) {
        return;
      }
      workspaces = result.workspaces;
      if (!workspaces.some((workspace) => workspace.id === workspaceId)) {
        workspaceId = workspaces[0]?.id ?? "";
      }
      await loadBoards(boardId);
      uncertain = false;
    } catch (cause) {
      error = asEnterpriseError(cause);
    } finally {
      busy = false;
    }
  }

  async function chooseWorkspace(id: string) {
    if (busy || commandPending) {
      return;
    }
    workspaceId = id;
    boards = [];
    boardId = "";
    busy = true;
    error = null;
    try {
      await loadBoards();
    } catch (cause) {
      error = asEnterpriseError(cause);
    } finally {
      busy = false;
    }
  }

  async function create(kind: "workspace" | "board") {
    if (busy || commandPending || uncertain) {
      return;
    }
    busy = true;
    error = null;
    try {
      if (kind === "workspace") {
        const workspace = await client.createWorkspace(workspaceName.trim());
        workspaceName = "";
        workspaces = (await client.workspaces()).workspaces;
        workspaceId = workspace.id;
        await loadBoards();
      } else {
        const board = await client.createBoard(workspaceId, boardName.trim());
        boardName = "";
        await loadBoards(board.id);
      }
    } catch (cause) {
      error = asEnterpriseError(cause);
      uncertain = error.outcomeUnknown;
    } finally {
      busy = false;
    }
  }

  onMount(() => void refresh());
  onDestroy(() => {
    disposed = true;
    boardEpoch++;
  });
</script>

<Card.Root>
  <Card.Header class="flex-row items-center justify-between gap-3">
    <Card.Title>{m.enterprise_workspace_board()}</Card.Title>
    <Button
      variant="outline"
      size="sm"
      disabled={busy || commandPending}
      onclick={refresh}
    >
      {m.enterprise_refresh()}
    </Button>
  </Card.Header>
  <Card.Content class="grid gap-4">
    <ErrorNotice {error} />
    {#if uncertain}
      <p role="status" class="text-sm text-muted-foreground">
        {m.enterprise_admin_uncertain()}
      </p>
    {/if}
    <div class="grid gap-4 sm:grid-cols-2">
      <FieldSelect
        id="enterprise-workspace"
        label={m.enterprise_workspace()}
        value={workspaceId}
        options={workspaces.map((workspace) => ({
          value: workspace.id,
          label: workspace.name,
        }))}
        disabled={busy || commandPending || workspaces.length === 0}
        onChange={(id) => void chooseWorkspace(id)}
      />
      <FieldSelect
        id="enterprise-board"
        label={m.enterprise_board()}
        bind:value={boardId}
        options={boards.map((board) => ({
          value: board.id,
          label: board.name,
        }))}
        disabled={busy || commandPending || boards.length === 0}
      />
    </div>
    {#if !busy && boards.length === 0}
      <p class="text-sm text-muted-foreground">{m.enterprise_no_boards()}</p>
    {/if}
    {#if canAdminister}
      <details class="rounded-md border p-3">
        <summary class="cursor-pointer text-sm font-medium"
          >{m.enterprise_create_resources()}</summary
        >
        <div class="mt-4 grid gap-4 sm:grid-cols-2">
          <form
            class="grid gap-2"
            onsubmit={(event) => {
              event.preventDefault();
              void create("workspace");
            }}
          >
            <Label for="enterprise-workspace-name"
              >{m.enterprise_workspace_name()}</Label
            >
            <Input
              id="enterprise-workspace-name"
              bind:value={workspaceName}
              required
              maxlength={200}
              disabled={busy || commandPending || uncertain}
            />
            <Button
              type="submit"
              variant="secondary"
              disabled={busy ||
                commandPending ||
                uncertain ||
                !workspaceName.trim()}>{m.enterprise_create_workspace()}</Button
            >
          </form>
          <form
            class="grid gap-2"
            onsubmit={(event) => {
              event.preventDefault();
              void create("board");
            }}
          >
            <Label for="enterprise-board-name"
              >{m.enterprise_board_name()}</Label
            >
            <Input
              id="enterprise-board-name"
              bind:value={boardName}
              required
              maxlength={200}
              disabled={busy || commandPending || uncertain || !workspaceId}
            />
            <Button
              type="submit"
              variant="secondary"
              disabled={busy ||
                commandPending ||
                uncertain ||
                !workspaceId ||
                !boardName.trim()}>{m.enterprise_create_board()}</Button
            >
          </form>
        </div>
      </details>
    {/if}
  </Card.Content>
</Card.Root>

<Tabs.Root bind:value={tab} class="gap-5">
  <ScrollArea orientation="horizontal" class="w-full">
    <Tabs.List aria-label={m.enterprise_sections()}>
      <Tabs.Trigger value="board">{m.enterprise_tasks()}</Tabs.Trigger>
      <Tabs.Trigger value="access" disabled={commandPending}
        >{m.enterprise_access()}</Tabs.Trigger
      >
      {#if canAdminister}
        <Tabs.Trigger value="members" disabled={commandPending}
          >{m.enterprise_members_devices()}</Tabs.Trigger
        >
        <Tabs.Trigger value="audit" disabled={commandPending}
          >{m.enterprise_audit()}</Tabs.Trigger
        >
      {/if}
    </Tabs.List>
  </ScrollArea>
  <Tabs.Content value="board">
    {#if selectedBoard}
      {#key selectedBoard.id}
        <BoardPanel
          {client}
          {session}
          board={selectedBoard}
          onUnavailable={removeUnavailableBoard}
          bind:commandPending
        />
      {/key}
    {:else}
      <p
        class="rounded-lg border border-dashed p-8 text-center text-sm text-muted-foreground"
      >
        {m.enterprise_select_board()}
      </p>
    {/if}
  </Tabs.Content>
  <Tabs.Content value="access">
    {#if selectedBoard}
      {#key selectedBoard.id}
        <AccessPanel {client} board={selectedBoard} onUpdated={refresh} />
      {/key}
    {:else}
      <p class="text-sm text-muted-foreground">{m.enterprise_select_board()}</p>
    {/if}
  </Tabs.Content>
  {#if canAdminister}
    <Tabs.Content value="members"
      ><MembersPanel {client} {session} /></Tabs.Content
    >
    <Tabs.Content value="audit"><AuditPanel {client} /></Tabs.Content>
  {/if}
</Tabs.Root>
