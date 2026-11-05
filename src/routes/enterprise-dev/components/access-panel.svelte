<script lang="ts">
  import { m } from "#lib/paraglide/messages.js";
  import { Button } from "#lib/components/ui/button/index.js";
  import { Input } from "#lib/components/ui/input/index.js";
  import { Label } from "#lib/components/ui/label/index.js";
  import * as Card from "#lib/components/ui/card/index.js";
  import {
    EnterpriseClient,
    EnterpriseError,
    asEnterpriseError,
  } from "#lib/enterprise/client.js";
  import type { Board, BoardRole } from "#lib/enterprise/types.js";
  import { roleLabel } from "#lib/enterprise/labels.js";
  import ErrorNotice from "./error-notice.svelte";
  import FieldSelect from "./field-select.svelte";

  let {
    client,
    board,
    onUpdated,
  }: {
    client: EnterpriseClient;
    board: Board;
    onUpdated: () => Promise<void>;
  } = $props();
  let memberId = $state("");
  let role = $state("viewer");
  let busy = $state(false);
  let saved = $state(false);
  let error = $state<EnterpriseError | null>(null);

  async function save() {
    if (busy || board.role !== "manager") {
      return;
    }
    busy = true;
    saved = false;
    error = null;
    try {
      const nextRole: BoardRole | null =
        role === "remove"
          ? null
          : role === "manager"
            ? "manager"
            : role === "editor"
              ? "editor"
              : "viewer";
      await client.setBoardAccess(board.id, memberId.trim(), nextRole);
      saved = true;
      await onUpdated();
    } catch (cause) {
      error = asEnterpriseError(cause);
    } finally {
      busy = false;
    }
  }
</script>

<Card.Root>
  <Card.Header
    ><Card.Title>{m.enterprise_access()}</Card.Title><Card.Description
      >{m.enterprise_access_help()}</Card.Description
    ></Card.Header
  >
  <Card.Content class="grid gap-4">
    <ErrorNotice {error} />
    {#if saved}<p role="status" class="text-sm">
        {m.enterprise_access_saved()}
      </p>{/if}
    {#if board.role === "manager"}
      <form
        class="grid max-w-xl gap-4"
        onsubmit={(event) => {
          event.preventDefault();
          void save();
        }}
      >
        <div class="grid gap-2">
          <Label for="enterprise-access-member"
            >{m.enterprise_member_id()}</Label
          ><Input
            id="enterprise-access-member"
            bind:value={memberId}
            required
            disabled={busy}
            spellcheck={false}
          />
          <p class="text-xs text-muted-foreground">
            {m.enterprise_member_id_help()}
          </p>
        </div>
        <FieldSelect
          id="enterprise-access-role"
          label={m.enterprise_board_role()}
          bind:value={role}
          options={[
            { value: "viewer", label: roleLabel("viewer") },
            { value: "editor", label: roleLabel("editor") },
            { value: "manager", label: roleLabel("manager") },
            { value: "remove", label: m.enterprise_remove_access() },
          ]}
          disabled={busy}
        />
        <Button
          type="submit"
          variant={role === "remove" ? "destructive" : "default"}
          class="justify-self-start"
          disabled={busy || !memberId.trim()}
          >{m.enterprise_save_access()}</Button
        >
      </form>
    {:else}<p class="text-sm text-muted-foreground">
        {m.enterprise_manager_required()}
      </p>{/if}
  </Card.Content>
</Card.Root>
