<script lang="ts">
  import * as Select from "#lib/components/ui/select/index.js";
  import { Label } from "#lib/components/ui/label/index.js";

  let {
    id,
    label,
    value = $bindable(""),
    options,
    disabled = false,
    onChange,
  }: {
    id: string;
    label: string;
    value?: string;
    options: { value: string; label: string }[];
    disabled?: boolean;
    onChange?: (value: string) => void;
  } = $props();
</script>

<div class="grid min-w-0 gap-2">
  <Label for={id}>{label}</Label>
  <Select.Root
    type="single"
    {value}
    {disabled}
    onValueChange={(next) => {
      value = next;
      onChange?.(next);
    }}
  >
    <Select.Trigger {id} class="w-full">
      <span class="truncate">
        {options.find((option) => option.value === value)?.label ?? label}
      </span>
    </Select.Trigger>
    <Select.Content>
      {#each options as option (option.value)}
        <Select.Item value={option.value}>{option.label}</Select.Item>
      {/each}
    </Select.Content>
  </Select.Root>
</div>
