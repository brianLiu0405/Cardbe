<script lang="ts">
  import { onMount } from "svelte";
  import { m } from "#lib/paraglide/messages.js";
  import { Button } from "#lib/components/ui/button/index.js";
  import { Input } from "#lib/components/ui/input/index.js";
  import { Textarea } from "#lib/components/ui/textarea/index.js";
  import { Label } from "#lib/components/ui/label/index.js";
  import { Badge } from "#lib/components/ui/badge/index.js";
  import * as Card from "#lib/components/ui/card/index.js";
  import * as Dialog from "#lib/components/ui/dialog/index.js";
  import {
    EnterpriseClient,
    EnterpriseError,
    asEnterpriseError,
  } from "#lib/enterprise/client.js";
  import { roleLabel } from "#lib/enterprise/labels.js";
  import type {
    Device,
    IssuedDevice,
    Member,
    Session,
  } from "#lib/enterprise/types.js";
  import ErrorNotice from "./error-notice.svelte";
  import FieldSelect from "./field-select.svelte";

  let { client, session }: { client: EnterpriseClient; session: Session } =
    $props();
  let members = $state<Member[]>([]);
  let devices = $state<Device[]>([]);
  let busy = $state(false);
  let uncertain = $state(false);
  let error = $state<EnterpriseError | null>(null);
  let name = $state("");
  let role = $state("member");
  let initialDevice = $state("");
  let deviceName = $state("");
  let deviceMember = $state("");
  let issued = $state<IssuedDevice | null>(null);
  let copied = $state(false);
  let copyFailed = $state(false);
  let confirmation = $state<{
    type: "suspend" | "activate" | "revoke";
    id: string;
    name: string;
  } | null>(null);

  async function loadLists() {
    members = (await client.members()).members;
    devices = (await client.devices()).devices;
    if (
      !members.some(
        (member) => member.id === deviceMember && member.status === "active",
      )
    ) {
      deviceMember =
        members.find((member) => member.status === "active")?.id ?? "";
    }
  }

  async function refresh() {
    if (busy) {
      return;
    }
    busy = true;
    error = null;
    try {
      await loadLists();
      uncertain = false;
    } catch (cause) {
      error = asEnterpriseError(cause);
    } finally {
      busy = false;
    }
  }

  async function mutate(action: () => Promise<void>) {
    if (busy || uncertain) {
      return;
    }
    busy = true;
    error = null;
    try {
      await action();
      await loadLists();
    } catch (cause) {
      error = asEnterpriseError(cause);
      uncertain = error.outcomeUnknown;
    } finally {
      busy = false;
    }
  }

  function showIssued(value: IssuedDevice) {
    issued = value;
    copied = false;
    copyFailed = false;
  }

  async function copyToken() {
    if (!issued) {
      return;
    }
    try {
      await navigator.clipboard.writeText(issued.token);
      copied = true;
      copyFailed = false;
    } catch {
      copyFailed = true;
    }
  }

  function confirmChange() {
    const action = confirmation;
    if (!action) {
      return;
    }
    confirmation = null;
    void mutate(async () => {
      if (action.type === "revoke") {
        await client.revokeDevice(action.id);
      } else {
        await client.setMemberStatus(
          action.id,
          action.type === "suspend" ? "suspended" : "active",
        );
      }
    });
  }

  onMount(() => void refresh());
</script>

<div class="grid gap-5">
  <div class="flex flex-wrap items-center justify-between gap-3">
    <h2 class="text-lg font-semibold">{m.enterprise_members_devices()}</h2>
    <Button variant="outline" size="sm" disabled={busy} onclick={refresh}
      >{m.enterprise_refresh()}</Button
    >
  </div>
  <ErrorNotice {error} />
  {#if uncertain}<p role="status" class="text-sm text-muted-foreground">
      {m.enterprise_admin_uncertain()}
    </p>{/if}

  <div class="grid gap-5 lg:grid-cols-2">
    <Card.Root>
      <Card.Header
        ><Card.Title>{m.enterprise_create_member()}</Card.Title
        ><Card.Description>{m.enterprise_create_member_help()}</Card.Description
        ></Card.Header
      >
      <Card.Content>
        <form
          class="grid gap-4"
          onsubmit={(event) => {
            event.preventDefault();
            void mutate(async () => {
              const result = await client.createMember(
                name.trim(),
                role === "admin" ? "admin" : "member",
                initialDevice.trim(),
              );
              name = "";
              initialDevice = "";
              showIssued(result);
            });
          }}
        >
          <div class="grid gap-2">
            <Label for="enterprise-member-name"
              >{m.enterprise_member_name()}</Label
            ><Input
              id="enterprise-member-name"
              bind:value={name}
              required
              maxlength={200}
              disabled={busy || uncertain}
            />
          </div>
          <FieldSelect
            id="enterprise-member-role"
            label={m.enterprise_organization_role()}
            bind:value={role}
            options={[
              { value: "member", label: roleLabel("member") },
              { value: "admin", label: roleLabel("admin") },
            ]}
            disabled={busy || uncertain}
          />
          <div class="grid gap-2">
            <Label for="enterprise-initial-device"
              >{m.enterprise_device_name()}</Label
            ><Input
              id="enterprise-initial-device"
              bind:value={initialDevice}
              required
              maxlength={200}
              disabled={busy || uncertain}
            />
          </div>
          <Button
            type="submit"
            disabled={busy ||
              uncertain ||
              !name.trim() ||
              !initialDevice.trim()}>{m.enterprise_create_member()}</Button
          >
        </form>
      </Card.Content>
    </Card.Root>
    <Card.Root>
      <Card.Header
        ><Card.Title>{m.enterprise_create_device()}</Card.Title
        ><Card.Description>{m.enterprise_create_device_help()}</Card.Description
        ></Card.Header
      >
      <Card.Content>
        <form
          class="grid gap-4"
          onsubmit={(event) => {
            event.preventDefault();
            void mutate(async () => {
              const result = await client.createDevice(
                deviceMember,
                deviceName.trim(),
              );
              deviceName = "";
              showIssued(result);
            });
          }}
        >
          <FieldSelect
            id="enterprise-device-member"
            label={m.enterprise_member()}
            bind:value={deviceMember}
            options={members
              .filter((member) => member.status === "active")
              .map((member) => ({ value: member.id, label: member.name }))}
            disabled={busy || uncertain}
          />
          <div class="grid gap-2">
            <Label for="enterprise-new-device"
              >{m.enterprise_device_name()}</Label
            ><Input
              id="enterprise-new-device"
              bind:value={deviceName}
              required
              maxlength={200}
              disabled={busy || uncertain}
            />
          </div>
          <Button
            type="submit"
            disabled={busy || uncertain || !deviceMember || !deviceName.trim()}
            >{m.enterprise_issue_token()}</Button
          >
        </form>
      </Card.Content>
    </Card.Root>
  </div>

  <Card.Root>
    <Card.Header><Card.Title>{m.enterprise_members()}</Card.Title></Card.Header>
    <Card.Content>
      <ul class="grid gap-3">
        {#each members as member (member.id)}
          <li
            class="flex flex-wrap items-center justify-between gap-3 rounded-md border p-3"
          >
            <div class="min-w-0 space-y-2">
              <div class="flex flex-wrap items-center gap-2">
                <span class="font-medium">{member.name}</span><Badge
                  variant="secondary">{roleLabel(member.role)}</Badge
                ><Badge variant="outline"
                  >{member.status === "active"
                    ? m.enterprise_active()
                    : m.enterprise_suspended()}</Badge
                >
              </div>
              <p class="break-all font-mono text-xs text-muted-foreground">
                {member.id}
              </p>
            </div>
            <Button
              variant={member.status === "active" ? "destructive" : "outline"}
              size="sm"
              disabled={busy || uncertain}
              onclick={() => {
                confirmation = {
                  type: member.status === "active" ? "suspend" : "activate",
                  id: member.id,
                  name: member.name,
                };
              }}
            >
              {member.status === "active"
                ? m.enterprise_suspend()
                : m.enterprise_activate()}
            </Button>
          </li>
        {:else}<li class="text-sm text-muted-foreground">
            {m.enterprise_loading()}
          </li>{/each}
      </ul>
    </Card.Content>
  </Card.Root>

  <Card.Root>
    <Card.Header
      ><Card.Title>{m.enterprise_devices()}</Card.Title><Card.Description
        >{m.enterprise_revocation_help()}</Card.Description
      ></Card.Header
    >
    <Card.Content>
      <ul class="grid gap-3">
        {#each devices as device (device.id)}
          <li
            class="flex flex-wrap items-center justify-between gap-3 rounded-md border p-3"
          >
            <div class="min-w-0 space-y-2">
              <div class="flex flex-wrap items-center gap-2">
                <span class="font-medium">{device.name}</span><Badge
                  variant="outline"
                  >{device.revoked
                    ? m.enterprise_revoked()
                    : m.enterprise_active()}</Badge
                >{#if device.id === session.device.id}<Badge variant="secondary"
                    >{m.enterprise_current_device()}</Badge
                  >{/if}
              </div>
              <p class="text-sm text-muted-foreground">
                {members.find((member) => member.id === device.user_id)?.name ??
                  device.user_id}
              </p>
              <p class="break-all font-mono text-xs text-muted-foreground">
                {device.id}
              </p>
            </div>
            <Button
              variant="destructive"
              size="sm"
              disabled={busy || uncertain || device.revoked}
              onclick={() => {
                confirmation = {
                  type: "revoke",
                  id: device.id,
                  name: device.name,
                };
              }}>{m.enterprise_revoke()}</Button
            >
          </li>
        {:else}<li class="text-sm text-muted-foreground">
            {m.enterprise_no_devices()}
          </li>{/each}
      </ul>
    </Card.Content>
  </Card.Root>
</div>

<Dialog.Root
  open={issued !== null}
  onOpenChange={(open) => {
    if (!open) {
      issued = null;
    }
  }}
>
  <Dialog.Content>
    <Dialog.Header
      ><Dialog.Title>{m.enterprise_token_issued()}</Dialog.Title
      ><Dialog.Description>{m.enterprise_token_once()}</Dialog.Description
      ></Dialog.Header
    >
    {#if issued}
      <Label for="enterprise-issued-token">{issued.device.name}</Label>
      <Textarea
        id="enterprise-issued-token"
        readonly
        value={issued.token}
        rows={4}
        class="break-all font-mono text-xs"
        spellcheck={false}
      />
      {#if copyFailed}<p role="alert" class="text-sm text-destructive">
          {m.enterprise_copy_failed()}
        </p>{/if}
      <Dialog.Footer
        ><Button
          variant="outline"
          onclick={() => {
            issued = null;
          }}>{m.enterprise_close()}</Button
        ><Button onclick={copyToken}
          >{copied ? m.enterprise_copied() : m.enterprise_copy_token()}</Button
        ></Dialog.Footer
      >
    {/if}
  </Dialog.Content>
</Dialog.Root>

<Dialog.Root
  open={confirmation !== null}
  onOpenChange={(open) => {
    if (!open) {
      confirmation = null;
    }
  }}
>
  <Dialog.Content>
    <Dialog.Header
      ><Dialog.Title>{m.enterprise_confirm_change()}</Dialog.Title
      ><Dialog.Description
        >{m.enterprise_confirm_member_action({
          name: confirmation?.name ?? "",
        })}</Dialog.Description
      ></Dialog.Header
    >
    <Dialog.Footer
      ><Button
        variant="outline"
        onclick={() => {
          confirmation = null;
        }}>{m.enterprise_cancel()}</Button
      ><Button
        variant={confirmation?.type === "activate" ? "default" : "destructive"}
        onclick={confirmChange}
        >{confirmation?.type === "activate"
          ? m.enterprise_activate()
          : confirmation?.type === "suspend"
            ? m.enterprise_suspend()
            : m.enterprise_revoke()}</Button
      ></Dialog.Footer
    >
  </Dialog.Content>
</Dialog.Root>
