<script setup lang="ts">
import { computed, ref } from 'vue'
import { Button, Checkbox, Column, DataTable, InputNumber, InputText, MultiSelect, Select } from 'primevue'
import { useI18n } from 'vue-i18n'
import type { NetworkConfig } from '../types/network'
import type { VpnConfig } from '../generated/proto/api_manage'

const { t } = useI18n()

const props = defineProps<{ network: NetworkConfig }>()

const ensureVpn = (): VpnConfig => {
  if (!props.network.vpn) {
    props.network.vpn = {
      enabled: false,
      exit_enabled: false,
      dns_protect: true,
      ipv6_protect: true,
      fail_mode: 'direct',
      exits: [],
      rules: [],
    }
  }
  props.network.vpn.exits ??= []
  props.network.vpn.rules ??= []
  return props.network.vpn
}

const vpn = computed(() => ensureVpn())

const failModes = computed(() => [
  { label: t('vpn_fail_mode_direct'), value: 'direct' },
  { label: t('vpn_fail_mode_block'), value: 'block' },
])

const actions = computed(() => [
  { label: t('vpn_action_exit'), value: 'exit' },
  { label: t('vpn_action_direct'), value: 'direct' },
  { label: t('vpn_action_block'), value: 'block' },
])

const exitOptions = computed(() =>
  vpn.value.exits.map(exit => ({ label: exit.name, value: exit.name })),
)

const validationError = ref('')

function validate() {
  const config = vpn.value
  const names = new Set<string>()
  for (const exit of config.exits) {
    if (!exit.name.trim()) {
      validationError.value = t('vpn_error_exit_name_empty')
      return
    }
    if (names.has(exit.name)) {
      validationError.value = t('vpn_error_exit_duplicated', [exit.name])
      return
    }
    names.add(exit.name)
    if (!exit.node?.trim() || !isIpOrEmpty(exit.node)) {
      validationError.value = t('vpn_error_exit_node_invalid', [exit.name])
      return
    }
  }
  for (const rule of config.rules) {
    if (!rule.cidr?.trim() || !isCidr(rule.cidr)) {
      validationError.value = t('vpn_error_rule_cidr_invalid', [rule.cidr ?? ''])
      return
    }
    if (rule.action === 'exit') {
      if (!rule.exit || !names.has(rule.exit)) {
        validationError.value = t('vpn_error_rule_exit_missing', [rule.cidr])
        return
      }
      for (const fallback of rule.fallback_exits ?? []) {
        if (!names.has(fallback)) {
          validationError.value = t('vpn_error_rule_exit_missing', [rule.cidr])
          return
        }
      }
    }
  }
  validationError.value = ''
}

function isIpOrEmpty(value: string): boolean {
  if (!value.trim())
    return true
  return /^(\d{1,3}\.){3}\d{1,3}$/.test(value.trim()) || value.trim().includes(':')
}

function isCidr(value: string): boolean {
  const v4 = /^(\d{1,3}\.){3}\d{1,3}\/\d{1,2}$/
  const v6 = /^[0-9a-fA-F:]+\/\d{1,3}$/
  return v4.test(value.trim()) || v6.test(value.trim())
}

function addExit() {
  const vpnConfig = ensureVpn()
  const index = vpnConfig.exits.length + 1
  vpnConfig.exits.push({ name: `exit-${index}`, node: '', note: undefined })
  validate()
}

function removeExit(index: number) {
  const removed = vpn.value.exits.splice(index, 1)[0]
  if (removed) {
    for (const rule of vpn.value.rules) {
      if (rule.exit === removed.name)
        rule.exit = undefined
      rule.fallback_exits = (rule.fallback_exits ?? []).filter(name => name !== removed.name)
    }
  }
  validate()
}

function addRule() {
  const vpnConfig = ensureVpn()
  vpnConfig.rules.push({
    cidr: '',
    action: 'exit',
    exit: vpnConfig.exits[0]?.name,
    fallback_exits: [],
    priority: undefined,
  })
  validate()
}

function removeRule(index: number) {
  vpn.value.rules.splice(index, 1)
  validate()
}

function moveRule(index: number, delta: number) {
  const rules = vpn.value.rules
  const target = index + delta
  if (target < 0 || target >= rules.length)
    return
  const [rule] = rules.splice(index, 1)
  rules.splice(target, 0, rule)
}

defineExpose({ validate })
</script>

<template>
  <div class="flex flex-col gap-4">
    <div class="flex flex-row gap-x-9 gap-y-3 flex-wrap w-full">
      <div class="flex items-center gap-2">
        <Checkbox :model-value="vpn.enabled" binary input-id="vpn_enabled" @update:model-value="vpn.enabled = $event" />
        <label for="vpn_enabled">{{ t('vpn_enabled') }}</label>
        <span class="pi pi-question-circle ml-1 self-center" v-tooltip="t('vpn_enabled_help')" />
      </div>
      <div class="flex items-center gap-2">
        <Checkbox :model-value="vpn.exit_enabled" binary input-id="vpn_exit_enabled"
          @update:model-value="vpn.exit_enabled = $event" />
        <label for="vpn_exit_enabled">{{ t('vpn_exit_enabled') }}</label>
        <span class="pi pi-question-circle ml-1 self-center" v-tooltip="t('vpn_exit_enabled_help')" />
      </div>
      <div class="flex items-center gap-2">
        <Checkbox :model-value="vpn.dns_protect" binary input-id="vpn_dns_protect"
          @update:model-value="vpn.dns_protect = $event" />
        <label for="vpn_dns_protect">{{ t('vpn_dns_protect') }}</label>
        <span class="pi pi-question-circle ml-1 self-center" v-tooltip="t('vpn_dns_protect_help')" />
      </div>
      <div class="flex items-center gap-2">
        <Checkbox :model-value="vpn.ipv6_protect" binary input-id="vpn_ipv6_protect"
          @update:model-value="vpn.ipv6_protect = $event" />
        <label for="vpn_ipv6_protect">{{ t('vpn_ipv6_protect') }}</label>
        <span class="pi pi-question-circle ml-1 self-center" v-tooltip="t('vpn_ipv6_protect_help')" />
      </div>
    </div>

    <div class="flex flex-row gap-x-9 flex-wrap w-full">
      <div class="flex flex-col gap-2">
        <div class="flex">
          <label for="vpn_fail_mode">{{ t('vpn_fail_mode') }}</label>
          <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('vpn_fail_mode_help')" />
        </div>
        <Select id="vpn_fail_mode" v-model="vpn.fail_mode" :options="failModes" option-label="label"
          option-value="value" class="w-64" />
      </div>
    </div>

    <div class="flex flex-col gap-2">
      <div class="flex items-center justify-between">
        <div class="flex">
          <label>{{ t('vpn_exits') }}</label>
          <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('vpn_exits_help')" />
        </div>
        <Button icon="pi pi-plus" :label="t('vpn_add_exit')" severity="secondary" text
          @click="addExit" />
      </div>
      <DataTable :value="vpn.exits" data-key="name" size="small" :empty="t('vpn_no_exits')">
        <Column field="name" :header="t('vpn_exit_name')" style="min-width: 10rem">
          <template #body="slotProps">
            <InputText v-model="slotProps.data.name" class="w-full" @update:model-value="validate" />
          </template>
        </Column>
        <Column field="node" :header="t('vpn_exit_node')" style="min-width: 12rem">
          <template #body="slotProps">
            <InputText v-model="slotProps.data.node" class="w-full" :placeholder="t('vpn_exit_node_placeholder')"
              @update:model-value="validate" />
          </template>
        </Column>
        <Column field="note" :header="t('vpn_exit_note')" style="min-width: 10rem">
          <template #body="slotProps">
            <InputText v-model="slotProps.data.note" class="w-full" />
          </template>
        </Column>
        <Column style="width: 4rem">
          <template #body="slotProps">
            <Button icon="pi pi-trash" text severity="danger" @click="removeExit(slotProps.index)" />
          </template>
        </Column>
      </DataTable>
    </div>

    <div class="flex flex-col gap-2">
      <div class="flex items-center justify-between">
        <div class="flex">
          <label>{{ t('vpn_rules') }}</label>
          <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('vpn_rules_help')" />
        </div>
        <Button icon="pi pi-plus" :label="t('vpn_add_rule')" severity="secondary" text
          @click="addRule" />
      </div>
      <DataTable :value="vpn.rules" data-key="cidr" size="small" :empty="t('vpn_no_rules')"
        scrollable scroll-height="24rem">
        <Column :header="t('vpn_rule_cidr')" style="min-width: 14rem">
          <template #body="slotProps">
            <InputText v-model="slotProps.data.cidr" class="w-full" placeholder="0.0.0.0/0"
              @update:model-value="validate" />
          </template>
        </Column>
        <Column :header="t('vpn_rule_action')" style="min-width: 8rem">
          <template #body="slotProps">
            <Select v-model="slotProps.data.action" :options="actions" option-label="label"
              option-value="value" class="w-full" @update:model-value="validate" />
          </template>
        </Column>
        <Column :header="t('vpn_rule_exit')" style="min-width: 10rem">
          <template #body="slotProps">
            <Select v-model="slotProps.data.exit" :options="exitOptions" option-label="label"
              option-value="value" class="w-full" :disabled="slotProps.data.action !== 'exit'"
              :placeholder="t('vpn_rule_exit_placeholder')" @update:model-value="validate" />
          </template>
        </Column>
        <Column :header="t('vpn_rule_fallbacks')" style="min-width: 12rem">
          <template #body="slotProps">
            <MultiSelect v-model="slotProps.data.fallback_exits" :options="exitOptions"
              option-label="label" option-value="value" class="w-full" display="chip"
              :disabled="slotProps.data.action !== 'exit'" :placeholder="t('vpn_rule_fallbacks_placeholder')"
              @update:model-value="validate" />
          </template>
        </Column>
        <Column :header="t('vpn_rule_priority')" style="min-width: 7rem">
          <template #body="slotProps">
            <InputNumber v-model="slotProps.data.priority" :format="false" :min="0" class="w-full" />
          </template>
        </Column>
        <Column style="width: 8rem">
          <template #body="slotProps">
            <div class="flex flex-row">
              <Button icon="pi pi-arrow-up" text @click="moveRule(slotProps.index, -1)" />
              <Button icon="pi pi-arrow-down" text @click="moveRule(slotProps.index, 1)" />
              <Button icon="pi pi-trash" text severity="danger" @click="removeRule(slotProps.index)" />
            </div>
          </template>
        </Column>
      </DataTable>
    </div>

    <small v-if="validationError" class="p-error">{{ validationError }}</small>
  </div>
</template>
