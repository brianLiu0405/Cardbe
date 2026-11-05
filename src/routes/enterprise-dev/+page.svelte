<script lang="ts">
  import { onDestroy } from "svelte";
  import ShieldCheckIcon from "@lucide/svelte/icons/shield-check";
  import { m } from "#lib/paraglide/messages.js";
  import { applyLanguagePreference, getLocale } from "#lib/i18n/index.js";
  import { Button } from "#lib/components/ui/button/index.js";
  import { Badge } from "#lib/components/ui/badge/index.js";
  import { Input } from "#lib/components/ui/input/index.js";
  import { Label } from "#lib/components/ui/label/index.js";
  import * as Card from "#lib/components/ui/card/index.js";
  import { ScrollArea } from "#lib/components/ui/scroll-area/index.js";
  import {
    EnterpriseClient,
    EnterpriseError,
    asEnterpriseError,
  } from "#lib/enterprise/client.js";
  import { roleLabel } from "#lib/enterprise/labels.js";
  import type { Health, Session } from "#lib/enterprise/types.js";
  import Console from "./components/console.svelte";
  import ErrorNotice from "./components/error-notice.svelte";

  let token = $state("");
  let connecting = $state(false);
  let error = $state<EnterpriseError | null>(null);
  let client = $state<EnterpriseClient | null>(null);
  let candidate: EnterpriseClient | null = null;
  let session = $state<Session | null>(null);
  let health = $state<Health | null>(null);

  function disconnect() {
    candidate?.dispose();
    candidate = null;
    client?.dispose();
    client = null;
    session = null;
    health = null;
    token = "";
  }

  async function connect() {
    if (connecting || !token.trim()) {
      return;
    }
    error = null;
    connecting = true;
    const next = new EnterpriseClient(token, (code) => {
      disconnect();
      error = new EnterpriseError(code);
    });
    candidate = next;
    token = "";
    try {
      const status = await next.health();
      if (status.mode !== "prototype" || status.storage !== "sqlite") {
        throw new EnterpriseError("INTERNAL_ERROR");
      }
      const identity = await next.me();
      if (candidate !== next) {
        return;
      }
      health = status;
      session = identity;
      client = next;
      candidate = null;
    } catch (cause) {
      next.dispose();
      if (candidate === next) {
        candidate = null;
      }
      const failure = asEnterpriseError(cause);
      if (failure.code !== "SESSION_ENDED") {
        error = failure;
      }
    } finally {
      connecting = false;
    }
  }

  onDestroy(disconnect);
</script>

<svelte:head>
  <title>{m.enterprise_title()} · Cardbe</title>
  <meta name="robots" content="noindex,nofollow" />
</svelte:head>

<ScrollArea class="h-dvh bg-background" orientation="vertical">
  <main class="mx-auto flex max-w-6xl flex-col gap-6 px-4 py-8 sm:px-8">
    <header class="flex flex-wrap items-start justify-between gap-4">
      <div class="flex items-start gap-3">
        <div class="rounded-lg bg-primary/10 p-2.5 text-primary">
          <ShieldCheckIcon class="size-6" />
        </div>
        <div class="space-y-2">
          <h1 class="text-2xl font-semibold tracking-tight">
            {m.enterprise_title()}
          </h1>
          <p class="text-sm text-muted-foreground">{m.enterprise_subtitle()}</p>
        </div>
      </div>
      <div class="flex items-center gap-2">
        <Badge variant="secondary">{m.enterprise_prototype()}</Badge>
        <Button
          variant="outline"
          size="sm"
          onclick={() =>
            applyLanguagePreference(getLocale() === "en" ? "zh-TW" : "en")}
          >{m.enterprise_switch_language()}</Button
        >
      </div>
    </header>

    <div class="rounded-lg border bg-muted/40 px-4 py-3 text-sm leading-6">
      <p class="font-medium">{m.enterprise_scope()}</p>
      <p class="text-muted-foreground">{m.enterprise_scope_limits()}</p>
    </div>

    <ErrorNotice {error} />

    {#if client && session && health}
      <Card.Root>
        <Card.Content
          class="flex flex-wrap items-center justify-between gap-4 pt-6"
        >
          <div class="min-w-0 space-y-1">
            <p class="font-medium">{session.organization.name}</p>
            <p class="text-sm text-muted-foreground">
              {session.user.name} · {roleLabel(session.user.role)} · {session
                .device.name}
            </p>
            <p class="break-all font-mono text-xs text-muted-foreground">
              {m.enterprise_actor_id()}: {session.user.id}
            </p>
          </div>
          <div class="flex items-center gap-3">
            <Badge variant="outline">SQLite · v{health.version}</Badge>
            <Button variant="outline" onclick={disconnect}>
              {m.enterprise_disconnect()}
            </Button>
          </div>
        </Card.Content>
      </Card.Root>
      {#key client}
        <Console {client} {session} />
      {/key}
    {:else}
      <Card.Root class="mx-auto w-full max-w-xl">
        <Card.Header>
          <Card.Title>{m.enterprise_connect_title()}</Card.Title>
          <Card.Description>{m.enterprise_connect_help()}</Card.Description>
        </Card.Header>
        <Card.Content>
          <form
            class="grid gap-4"
            onsubmit={(event) => {
              event.preventDefault();
              void connect();
            }}
          >
            <div class="grid gap-2">
              <Label for="enterprise-token">{m.enterprise_token()}</Label>
              <Input
                id="enterprise-token"
                type="password"
                autocomplete="off"
                spellcheck={false}
                bind:value={token}
                disabled={connecting}
                required
              />
            </div>
            <p class="text-xs leading-5 text-muted-foreground">
              {m.enterprise_memory_only()}
            </p>
            <Button type="submit" disabled={connecting || !token.trim()}>
              {connecting ? m.enterprise_connecting() : m.enterprise_connect()}
            </Button>
          </form>
        </Card.Content>
      </Card.Root>
    {/if}
  </main>
</ScrollArea>
