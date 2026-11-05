<script lang="ts">
  import type { EnterpriseError } from "#lib/enterprise/client.js";
  import { errorLabel } from "#lib/enterprise/labels.js";

  let { error }: { error: EnterpriseError | null } = $props();
</script>

{#if error && error.code !== "SESSION_ENDED"}
  <p
    role="alert"
    class="rounded-md border border-destructive/30 bg-destructive/5 px-4 py-3 text-sm text-destructive"
  >
    {errorLabel(error.code)}
  </p>
{/if}
