<script setup lang="ts">
import { computed } from 'vue'
import { v4 as uuidv4 } from 'uuid'
import { Button, InputText } from 'primevue'
import { useI18n } from 'vue-i18n'
import type { NetworkConfig } from '../types/network'
import type { VpnConfig } from '../generated/proto/api_manage'

const { t } = useI18n()
const props = defineProps<{ network: NetworkConfig }>()
type VpnRule = VpnConfig['rules'][number]
const ruleKeys = new WeakMap<VpnRule, string>()
const vpn = computed(() => props.network.vpn ??= { exit_enabled: true, rules: [] })
function ruleKey(rule: VpnRule): string {
  if (!ruleKeys.has(rule)) ruleKeys.set(rule, uuidv4())
  return ruleKeys.get(rule)!
}
function addRule() {
  vpn.value.rules.push({ cidr: '', exit_addr: '' })
}
</script>

<template>
  <div class="flex min-w-0 flex-col gap-3">
    <div class="flex min-w-0 flex-wrap items-center justify-between gap-2">
      <label>{{ t('vpn_rules') }}</label>
      <Button icon="pi pi-plus" :label="t('vpn_add_rule')" severity="secondary" text @click="addRule" />
    </div>
    <div v-if="vpn.rules.length === 0" class="text-sm text-surface-500 dark:text-surface-400">
      {{ t('vpn_no_rules') }}
    </div>
    <div v-for="(rule, index) in vpn.rules" :key="ruleKey(rule)" data-testid="vpn-rule-card"
      class="grid min-w-0 grid-cols-1 gap-3 rounded border border-surface-200 p-3 dark:border-surface-700 sm:grid-cols-[1fr_1fr_auto]">
      <div class="min-w-0">
        <label :for="`vpn_rule_cidr_${index}`" class="mb-1 block">{{ t('vpn_rule_cidr') }}</label>
        <InputText :id="`vpn_rule_cidr_${index}`" v-model="rule.cidr" class="w-full min-w-0" placeholder="203.0.113.0/24" />
      </div>
      <div class="min-w-0">
        <label :for="`vpn_rule_exit_${index}`" class="mb-1 block">{{ t('vpn_rule_exit') }}</label>
        <InputText :id="`vpn_rule_exit_${index}`" v-model="rule.exit_addr" class="w-full min-w-0" placeholder="10.144.144.2" />
      </div>
      <Button class="self-end justify-self-end" icon="pi pi-trash" text severity="danger" :aria-label="t('vpn_rule_remove')"
        @click="vpn.rules.splice(index, 1)" />
    </div>
  </div>
</template>
