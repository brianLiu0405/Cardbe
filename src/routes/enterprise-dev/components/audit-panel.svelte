<script lang="ts">
  import { onMount } from "svelte";
  import { m } from "#lib/paraglide/messages.js";
  import { formatDateTime } from "#lib/i18n/index.js";
  import { Button } from "#lib/components/ui/button/index.js";
  import { ScrollArea } from "#lib/components/ui/scroll-area/index.js";
  import * as Card from "#lib/components/ui/card/index.js";
  import {
    EnterpriseClient,
    EnterpriseError,
    asEnterpriseError,
  } from "#lib/enterprise/client.js";
  import type { AuditEvent } from "#lib/enterprise/types.js";
  import ErrorNotice from "./error-notice.svelte";

  let { client }: { client: EnterpriseClient } = $props();
  let events = $state<AuditEvent[]>([]);
  let cursor = $state(0);
  let busy = $state(false);
  let exhausted = $state(false);
  let error = $state<EnterpriseError | null>(null);

  async function load(reset = false) {
    if (busy) {
      return;
    }
    busy = true;
    error = null;
    try {
      const page = await client.audit(reset ? 0 : cursor);
      const known = new Set(reset ? [] : events.map((event) => event.id));
      events = [
        ...(reset ? [] : events),
        ...page.events.filter((event) => !known.has(event.id)),
      ];
      cursor = page.next_cursor;
      exhausted = page.events.length < 50;
    } catch (cause) {
      error = asEnterpriseError(cause);
    } finally {
      busy = false;
    }
  }

  onMount(() => void load(true));
</script>

<Card.Root>
  <Card.Header class="flex-row items-start justify-between gap-3">
    <div class="grid gap-2">
      <Card.Title>{m.enterprise_audit()}</Card.Title><Card.Description
        >{m.enterprise_audit_help()}</Card.Description
      >
    </div>
    <Button
      variant="outline"
      size="sm"
      disabled={busy}
      onclick={() => void load(true)}>{m.enterprise_refresh()}</Button
    >
  </Card.Header>
  <Card.Content class="grid min-w-0 gap-4">
    <ErrorNotice {error} />
    <ScrollArea orientation="horizontal" class="w-full">
      <table class="w-full min-w-200 text-start text-sm">
        <thead
          ><tr class="border-b text-left text-muted-foreground"
            ><th scope="col" class="p-3 font-medium"
              >{m.enterprise_audit_time()}</th
            ><th scope="col" class="p-3 font-medium"
              >{m.enterprise_audit_action()}</th
            ><th scope="col" class="p-3 font-medium"
              >{m.enterprise_audit_actor()}</th
            ><th scope="col" class="p-3 font-medium"
              >{m.enterprise_audit_resource()}</th
            ><th scope="col" class="p-3 font-medium"
              >{m.enterprise_audit_outcome()}</th
            ></tr
          ></thead
        >
        <tbody>
          {#each events as event (event.id)}
            <tr class="border-b align-top last:border-0">
              <td class="whitespace-nowrap p-3 text-xs"
                >{formatDateTime(new Date(event.created_at))}</td
              >
              <td class="p-3 font-mono text-xs">{event.action}</td>
              <td class="p-3"
                ><div class="break-all font-mono text-xs">{event.actor_id}</div>
                <div
                  class="mt-1 break-all font-mono text-xs text-muted-foreground"
                >
                  {event.device_id}
                </div></td
              >
              <td class="p-3"
                ><div class="break-all font-mono text-xs">
                  {event.resource_id}
                </div>
                {#if event.operation_id}<div
                    class="mt-1 break-all font-mono text-xs text-muted-foreground"
                  >
                    {event.operation_id}
                  </div>{/if}</td
              >
              <td class="p-3 font-mono text-xs">{event.outcome}</td>
            </tr>
          {:else}<tr
              ><td colspan="5" class="p-8 text-center text-muted-foreground"
                >{busy ? m.enterprise_loading() : m.enterprise_no_audit()}</td
              ></tr
            >{/each}
        </tbody>
      </table>
    </ScrollArea>
    <Button
      variant="outline"
      class="justify-self-start"
      disabled={busy || exhausted}
      onclick={() => void load()}>{m.enterprise_load_more()}</Button
    >
  </Card.Content>
</Card.Root>
