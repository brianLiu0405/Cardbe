<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import { m } from "#lib/paraglide/messages.js";
  import { formatDateTime, formatNumber } from "#lib/i18n/index.js";
  import { confirmUnsavedChanges } from "#lib/unsaved-changes.js";
  import { Button } from "#lib/components/ui/button/index.js";
  import { Input } from "#lib/components/ui/input/index.js";
  import { Textarea } from "#lib/components/ui/textarea/index.js";
  import { Label } from "#lib/components/ui/label/index.js";
  import { Badge } from "#lib/components/ui/badge/index.js";
  import * as Card from "#lib/components/ui/card/index.js";
  import {
    EnterpriseClient,
    EnterpriseError,
    asEnterpriseError,
    prepareCommand,
  } from "#lib/enterprise/client.js";
  import { roleLabel } from "#lib/enterprise/labels.js";
  import type {
    Board,
    Command,
    CommandResult,
    Member,
    PendingCommand,
    Session,
    Snapshot,
    Task,
  } from "#lib/enterprise/types.js";
  import ErrorNotice from "./error-notice.svelte";
  import FieldSelect from "./field-select.svelte";

  let {
    client,
    board,
    session,
    onUnavailable,
    commandPending = $bindable(false),
  }: {
    client: EnterpriseClient;
    board: Board;
    session: Session;
    onUnavailable: (boardId: string) => void;
    commandPending?: boolean;
  } = $props();

  let snapshot = $state<Snapshot | null>(null);
  let members = $state<Member[]>([]);
  let loading = $state(true);
  let busy = $state(false);
  let error = $state<EnterpriseError | null>(null);
  let pending = $state<PendingCommand | null>(null);
  let lastResult = $state<CommandResult | null>(null);
  let selectedTaskId = $state("");
  let createTitle = $state("");
  let createDescription = $state("");
  let createAssignee = $state("none");
  let editTitle = $state("");
  let editDescription = $state("");
  let editStatus = $state("todo");
  let editAssignee = $state("none");
  let commentBody = $state("");
  let unavailable = $state(false);
  let epoch = 0;
  let snapshotRequest = 0;
  let disposed = false;
  const selectedTask = $derived(
    snapshot?.tasks.find((task) => task.id === selectedTaskId),
  );
  const canWrite = $derived(
    !unavailable && snapshot !== null && snapshot.board.role !== "viewer",
  );
  const comments = $derived(
    snapshot?.comments.filter(
      (comment) => comment.task_id === selectedTaskId,
    ) ?? [],
  );
  const draftChanged = $derived(
    selectedTask !== undefined &&
      (editTitle !== selectedTask.title ||
        editDescription !== selectedTask.description ||
        editStatus !== selectedTask.status ||
        (editAssignee === "none" ? null : editAssignee) !==
          selectedTask.assignee_id),
  );
  const assigneeOptions = $derived.by(() => {
    const candidates = new Map<string, string>();
    candidates.set(session.user.id, session.user.name);
    for (const member of members) {
      if (member.status === "active") {
        candidates.set(member.id, member.name);
      }
    }
    // Preserve an existing assignee even if this actor cannot read the directory.
    if (
      selectedTask?.assignee_id &&
      !candidates.has(selectedTask.assignee_id)
    ) {
      candidates.set(selectedTask.assignee_id, selectedTask.assignee_id);
    }
    return [
      { value: "none", label: m.enterprise_unassigned() },
      ...[...candidates].map(([value, label]) => ({ value, label })),
    ];
  });

  function setEditor(task: Task | undefined) {
    editTitle = task?.title ?? "";
    editDescription = task?.description ?? "";
    editStatus = task?.status ?? "todo";
    editAssignee = task?.assignee_id ?? "none";
  }

  function isCurrent(generation: number) {
    return !disposed && !unavailable && generation === epoch;
  }

  function invalidateBoard() {
    if (disposed || unavailable) {
      return;
    }
    unavailable = true;
    epoch++;
    snapshot = null;
    members = [];
    selectedTaskId = "";
    createTitle = "";
    createDescription = "";
    createAssignee = "none";
    setEditor(undefined);
    commentBody = "";
    pending = null;
    lastResult = null;
    commandPending = false;
    loading = false;
    busy = false;
    error = new EnterpriseError("NOT_FOUND");
    onUnavailable(board.id);
  }

  async function loadSnapshot(preserveDraft = false) {
    const generation = epoch;
    const request = ++snapshotRequest;
    let next: Snapshot;
    try {
      next = await client.snapshot(board.id);
    } catch (cause) {
      // A missing snapshot means this board is inaccessible or deleted. A
      // command's NOT_FOUND can instead mean only a task has disappeared.
      if (
        isCurrent(generation) &&
        asEnterpriseError(cause).code === "NOT_FOUND"
      ) {
        invalidateBoard();
      }
      throw cause;
    }
    if (!isCurrent(generation) || request !== snapshotRequest) {
      return;
    }
    snapshot = next;
    if (!next.tasks.some((task) => task.id === selectedTaskId)) {
      selectedTaskId = next.tasks[0]?.id ?? "";
      preserveDraft = false;
    }
    if (!preserveDraft) {
      setEditor(next.tasks.find((task) => task.id === selectedTaskId));
    }
  }

  async function refresh() {
    if (busy || pending || loading || unavailable || disposed) {
      return;
    }
    const generation = epoch;
    if (
      !(await confirmUnsavedChanges(() => draftChanged || !!commentBody.trim()))
    ) {
      return;
    }
    if (!isCurrent(generation) || busy || pending || loading) {
      return;
    }
    loading = true;
    error = null;
    try {
      await loadSnapshot();
      if (isCurrent(generation)) {
        commentBody = "";
      }
    } catch (cause) {
      if (isCurrent(generation)) {
        error = asEnterpriseError(cause);
      }
    } finally {
      if (isCurrent(generation)) {
        loading = false;
      }
    }
  }

  async function selectTask(id: string) {
    if (
      id === selectedTaskId ||
      busy ||
      pending ||
      loading ||
      unavailable ||
      disposed
    ) {
      return;
    }
    const generation = epoch;
    if (
      !(await confirmUnsavedChanges(() => draftChanged || !!commentBody.trim()))
    ) {
      return;
    }
    if (!isCurrent(generation) || busy || pending || loading) {
      return;
    }
    selectedTaskId = id;
    setEditor(snapshot?.tasks.find((task) => task.id === id));
    commentBody = "";
  }

  async function send(attempt: PendingCommand) {
    if (busy || unavailable || disposed) {
      return;
    }
    const generation = epoch;
    pending = attempt;
    commandPending = true;
    busy = true;
    error = null;
    lastResult = null;
    try {
      const result = await client.submit(attempt);
      if (!isCurrent(generation)) {
        return;
      }
      lastResult = result;
      pending = null;
      commandPending = false;
      const command = attempt.request.command;
      if (command.type === "task.create") {
        createTitle = "";
        createDescription = "";
        createAssignee = "none";
      } else if (command.type === "comment.create") {
        commentBody = "";
      }
      // The mutation is confirmed even if this following read fails.
      await loadSnapshot(command.type !== "task.update" && draftChanged);
    } catch (cause) {
      if (isCurrent(generation)) {
        error = asEnterpriseError(cause);
      }
    } finally {
      if (isCurrent(generation)) {
        busy = false;
      }
    }
  }

  function submit(command: Command) {
    if (!canWrite || pending || busy || loading) {
      return;
    }
    void send(prepareCommand(board.id, command));
  }

  function updateTask() {
    if (!selectedTask) {
      return;
    }
    submit({
      type: "task.update",
      task_id: selectedTask.id,
      base_version: selectedTask.version,
      title: editTitle.trim(),
      description: editDescription,
      status: editStatus === "done" ? "done" : "todo",
      assignee_id: editAssignee === "none" ? null : editAssignee,
    });
  }

  async function reviseRejected() {
    if (
      !pending ||
      !error ||
      error.outcomeUnknown ||
      busy ||
      unavailable ||
      disposed
    ) {
      return;
    }
    const generation = epoch;
    const command = pending.request.command;
    pending = null;
    commandPending = false;
    loading = true;
    error = null;
    try {
      // Keep rejected draft text; refresh the base version before explicit resubmission.
      await loadSnapshot(command.type !== "task.create");
    } catch (cause) {
      if (isCurrent(generation)) {
        error = asEnterpriseError(cause);
      }
    } finally {
      if (isCurrent(generation)) {
        loading = false;
      }
    }
  }

  onMount(() => {
    const generation = epoch;
    void (async () => {
      try {
        await loadSnapshot();
        if (isCurrent(generation) && session.user.role !== "member") {
          const directory = await client.members();
          if (isCurrent(generation)) {
            members = directory.members;
          }
        }
      } catch (cause) {
        if (isCurrent(generation)) {
          error = asEnterpriseError(cause);
        }
      } finally {
        if (isCurrent(generation)) {
          loading = false;
        }
      }
    })();
  });

  onDestroy(() => {
    disposed = true;
    epoch++;
  });
</script>

<div class="grid gap-5">
  <div class="flex flex-wrap items-center justify-between gap-3">
    <div class="flex flex-wrap items-center gap-2">
      <h2 class="text-lg font-semibold">{board.name}</h2>
      <Badge variant="secondary"
        >{roleLabel(snapshot?.board.role ?? board.role)}</Badge
      >
      {#if snapshot}
        <span class="text-xs text-muted-foreground"
          >{m.enterprise_revision({
            version: formatNumber(snapshot.board.revision),
          })}</span
        >
      {/if}
    </div>
    <Button
      variant="outline"
      size="sm"
      disabled={busy || !!pending || loading}
      onclick={refresh}>{m.enterprise_refresh()}</Button
    >
  </div>

  <ErrorNotice {error} />
  {#if loading}<p class="text-sm text-muted-foreground" role="status">
      {m.enterprise_loading()}
    </p>{/if}
  {#if snapshot && !canWrite}<p class="text-sm text-muted-foreground">
      {m.enterprise_viewer_help()}
    </p>{/if}

  {#if pending}
    <Card.Root class="border-primary/30">
      <Card.Header>
        <Card.Title
          >{busy
            ? m.enterprise_sending()
            : m.enterprise_pending_title()}</Card.Title
        >
        <Card.Description>{m.enterprise_pending_help()}</Card.Description>
      </Card.Header>
      <Card.Content class="grid gap-3">
        <p class="break-all font-mono text-xs">
          {pending.request.operation_id} · {pending.request.command.type}
        </p>
        <div class="flex flex-wrap gap-2">
          <Button disabled={busy} onclick={() => pending && void send(pending)}
            >{m.enterprise_retry_same()}</Button
          >
          {#if error && !error.outcomeUnknown}
            <Button variant="outline" disabled={busy} onclick={reviseRejected}
              >{m.enterprise_revise_rejected()}</Button
            >
          {/if}
        </div>
      </Card.Content>
    </Card.Root>
  {/if}
  {#if lastResult}
    <p role="status" class="rounded-md border bg-muted/30 px-4 py-3 text-sm">
      {lastResult.replayed
        ? m.enterprise_result_replayed()
        : m.enterprise_result_accepted()}
      <span class="text-muted-foreground"
        >{m.enterprise_revision({
          version: formatNumber(lastResult.revision),
        })}</span
      >
    </p>
  {/if}

  {#if canWrite}
    <Card.Root>
      <Card.Header
        ><Card.Title>{m.enterprise_new_task()}</Card.Title></Card.Header
      >
      <Card.Content>
        <form
          class="grid gap-4"
          onsubmit={(event) => {
            event.preventDefault();
            submit({
              type: "task.create",
              title: createTitle.trim(),
              description: createDescription,
              assignee_id: createAssignee === "none" ? null : createAssignee,
            });
          }}
        >
          <div class="grid gap-4 sm:grid-cols-2">
            <div class="grid gap-2">
              <Label for="enterprise-task-title"
                >{m.enterprise_task_title()}</Label
              ><Input
                id="enterprise-task-title"
                bind:value={createTitle}
                required
                maxlength={500}
                disabled={busy || !!pending || loading}
              />
            </div>
            <FieldSelect
              id="enterprise-create-assignee"
              label={m.enterprise_assignee()}
              bind:value={createAssignee}
              options={assigneeOptions}
              disabled={busy || !!pending || loading}
            />
          </div>
          <div class="grid gap-2">
            <Label for="enterprise-task-description"
              >{m.enterprise_description()}</Label
            ><Textarea
              id="enterprise-task-description"
              bind:value={createDescription}
              rows={2}
              disabled={busy || !!pending || loading}
            />
          </div>
          <p class="text-xs text-muted-foreground">
            {m.enterprise_assignee_help()}
          </p>
          <Button
            type="submit"
            class="justify-self-start"
            disabled={busy || !!pending || loading || !createTitle.trim()}
            >{m.enterprise_create_task()}</Button
          >
        </form>
      </Card.Content>
    </Card.Root>
  {/if}

  {#if snapshot}
    <div
      class="grid items-start gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(0,2fr)]"
    >
      <Card.Root>
        <Card.Header
          ><Card.Title
            >{m.enterprise_task_count({
              count: formatNumber(snapshot.tasks.length),
            })}</Card.Title
          ></Card.Header
        >
        <Card.Content class="grid gap-2">
          {#each snapshot.tasks as task (task.id)}
            <Button
              variant={task.id === selectedTaskId ? "secondary" : "ghost"}
              class="h-auto min-h-11 justify-start whitespace-normal py-2 text-start"
              aria-pressed={task.id === selectedTaskId}
              disabled={busy || !!pending || loading}
              onclick={() => void selectTask(task.id)}
            >
              <span class="min-w-0 break-words"
                >{task.title}<span
                  class="mt-1 block text-xs font-normal text-muted-foreground"
                  >{task.status === "done"
                    ? m.enterprise_done()
                    : m.enterprise_todo()}</span
                ></span
              >
            </Button>
          {:else}<p class="text-sm text-muted-foreground">
              {m.enterprise_no_tasks()}
            </p>{/each}
        </Card.Content>
      </Card.Root>

      {#if selectedTask}
        <div class="grid gap-5">
          <Card.Root>
            <Card.Header
              ><Card.Title>{m.enterprise_task_details()}</Card.Title
              ><Card.Description
                >{m.enterprise_task_version({
                  version: formatNumber(selectedTask.version),
                })}</Card.Description
              ></Card.Header
            >
            <Card.Content>
              <form
                class="grid gap-4"
                onsubmit={(event) => {
                  event.preventDefault();
                  updateTask();
                }}
              >
                <div class="grid gap-2">
                  <Label for="enterprise-edit-title"
                    >{m.enterprise_task_title()}</Label
                  ><Input
                    id="enterprise-edit-title"
                    bind:value={editTitle}
                    required
                    maxlength={500}
                    disabled={!canWrite || busy || !!pending || loading}
                  />
                </div>
                <div class="grid gap-2">
                  <Label for="enterprise-edit-description"
                    >{m.enterprise_description()}</Label
                  ><Textarea
                    id="enterprise-edit-description"
                    bind:value={editDescription}
                    rows={4}
                    disabled={!canWrite || busy || !!pending || loading}
                  />
                </div>
                <div class="grid gap-4 sm:grid-cols-2">
                  <FieldSelect
                    id="enterprise-edit-status"
                    label={m.enterprise_task_status()}
                    bind:value={editStatus}
                    options={[
                      { value: "todo", label: m.enterprise_todo() },
                      { value: "done", label: m.enterprise_done() },
                    ]}
                    disabled={!canWrite || busy || !!pending || loading}
                  />
                  <FieldSelect
                    id="enterprise-edit-assignee"
                    label={m.enterprise_assignee()}
                    bind:value={editAssignee}
                    options={assigneeOptions}
                    disabled={!canWrite || busy || !!pending || loading}
                  />
                </div>
                <p class="break-all font-mono text-xs text-muted-foreground">
                  {selectedTask.id}
                </p>
                {#if canWrite}<Button
                    type="submit"
                    class="justify-self-start"
                    disabled={busy ||
                      !!pending ||
                      loading ||
                      !editTitle.trim() ||
                      !draftChanged}>{m.enterprise_save_task()}</Button
                  >{/if}
              </form>
            </Card.Content>
          </Card.Root>
          <Card.Root>
            <Card.Header
              ><Card.Title>{m.enterprise_comments()}</Card.Title></Card.Header
            >
            <Card.Content class="grid gap-4">
              {#each comments as comment (comment.id)}
                <article class="space-y-2 rounded-md border p-3">
                  <p class="break-all text-xs text-muted-foreground">
                    {comment.author_id} · {formatDateTime(
                      new Date(comment.created_at),
                    )}
                  </p>
                  <p class="whitespace-pre-wrap break-words text-sm">
                    {comment.body}
                  </p>
                </article>
              {:else}<p class="text-sm text-muted-foreground">
                  {m.enterprise_no_comments()}
                </p>{/each}
              {#if canWrite}
                <form
                  class="grid gap-3"
                  onsubmit={(event) => {
                    event.preventDefault();
                    if (selectedTask) {
                      submit({
                        type: "comment.create",
                        task_id: selectedTask.id,
                        body: commentBody.trim(),
                      });
                    }
                  }}
                >
                  <Label for="enterprise-comment"
                    >{m.enterprise_new_comment()}</Label
                  >
                  <Textarea
                    id="enterprise-comment"
                    bind:value={commentBody}
                    rows={3}
                    required
                    disabled={busy || !!pending || loading}
                  />
                  <Button
                    type="submit"
                    class="justify-self-start"
                    disabled={busy ||
                      !!pending ||
                      loading ||
                      !commentBody.trim()}>{m.enterprise_add_comment()}</Button
                  >
                </form>
              {/if}
            </Card.Content>
          </Card.Root>
        </div>
      {/if}
    </div>
  {/if}
</div>
