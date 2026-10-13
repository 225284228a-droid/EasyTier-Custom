<script setup lang="ts">
import { computed } from 'vue'
import { v4 as uuidv4 } from 'uuid'
import { Button, Checkbox, InputNumber, InputText, Select } from 'primevue'
import { useI18n } from 'vue-i18n'
import type { NetworkConfig } from '../types/network'
import type { VpnConfig } from '../generated/proto/api_manage'

const { t } = useI18n()

const props = defineProps<{ network: NetworkConfig }>()

type VpnRule = VpnConfig['rules'][number]

const vpnRuleViewKeys = new WeakMap<VpnRule, string>()
const vpnRuleExitViewKeys = new WeakMap<VpnRule, string[]>()

const ensureVpn = (): VpnConfig => {
  if (!props.network.vpn) {
    props.network.vpn = {
      exit_enabled: false,
      dns_protect: true,
      ipv6_protect: true,
      ignore_private: true,
      rules: [],
    }
  }
  const config = props.network.vpn
  config.dns_protect ??= true
  config.ipv6_protect ??= true
  config.ignore_private ??= true
  config.rules ??= []
  for (const rule of config.rules)
    rule.exits ??= []
  return config
}

const vpn = computed(() => ensureVpn())

const actions = computed(() => [
  { label: t('vpn_action_exit'), value: 'exit' },
  { label: t('vpn_action_direct'), value: 'direct' },
  { label: t('vpn_action_block'), value: 'block' },
])

function vpnRuleViewKey(rule: VpnRule): string {
  let key = vpnRuleViewKeys.get(rule)
  if (!key) {
    key = uuidv4()
    vpnRuleViewKeys.set(rule, key)
  }
  return key
}

function vpnRuleExitKeys(rule: VpnRule): string[] {
  let keys = vpnRuleExitViewKeys.get(rule)
  if (!keys) {
    keys = []
    vpnRuleExitViewKeys.set(rule, keys)
  }
  if (keys.length > rule.exits.length)
    keys.length = rule.exits.length
  while (keys.length < rule.exits.length)
    keys.push(uuidv4())
  return keys
}

function addRule() {
  vpn.value.rules.push({
    cidr: '',
    action: 'exit',
    exits: [],
    priority: undefined,
  })
}

function removeRule(index: number) {
  vpn.value.rules.splice(index, 1)
}

function moveRule(index: number, delta: number) {
  const rules = vpn.value.rules
  const target = index + delta
  if (target < 0 || target >= rules.length)
    return
  const [rule] = rules.splice(index, 1)
  rules.splice(target, 0, rule)
}

function addRuleExit(rule: VpnRule) {
  rule.exits.push('')
  vpnRuleExitKeys(rule)
}

function removeRuleExit(rule: VpnRule, index: number) {
  const keys = vpnRuleExitKeys(rule)
  rule.exits.splice(index, 1)
  keys.splice(index, 1)
}

function moveRuleExit(rule: VpnRule, index: number, delta: number) {
  const target = index + delta
  if (target < 0 || target >= rule.exits.length)
    return
  const [exit] = rule.exits.splice(index, 1)
  rule.exits.splice(target, 0, exit)
  const keys = vpnRuleExitKeys(rule)
  const [key] = keys.splice(index, 1)
  keys.splice(target, 0, key)
}
</script>

<template>
  <div class="flex min-w-0 flex-col gap-4">
    <div class="flex flex-row flex-wrap">
      <div class="basis-[20rem] flex min-w-0 items-center">
        <Checkbox :model-value="vpn.dns_protect" binary input-id="vpn_dns_protect"
          @update:model-value="vpn.dns_protect = $event" />
        <label for="vpn_dns_protect" class="ml-2">{{ t('vpn_dns_protect') }}</label>
        <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('vpn_dns_protect_help')" />
      </div>
      <div class="basis-[20rem] flex min-w-0 items-center">
        <Checkbox :model-value="vpn.ipv6_protect" binary input-id="vpn_ipv6_protect"
          @update:model-value="vpn.ipv6_protect = $event" />
        <label for="vpn_ipv6_protect" class="ml-2">{{ t('vpn_ipv6_protect') }}</label>
        <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('vpn_ipv6_protect_help')" />
      </div>
      <div class="basis-[20rem] flex min-w-0 items-center">
        <Checkbox :model-value="vpn.ignore_private" binary input-id="vpn_ignore_private"
          @update:model-value="vpn.ignore_private = $event" />
        <label for="vpn_ignore_private" class="ml-2">{{ t('vpn_ignore_private') }}</label>
        <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('vpn_ignore_private_help')" />
      </div>
    </div>

    <div class="flex min-w-0 flex-col gap-2">
      <div class="flex min-w-0 flex-wrap items-center justify-between gap-2">
        <div class="flex min-w-0 flex-wrap items-center">
          <label>{{ t('vpn_rules') }}</label>
          <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('vpn_rules_help')" />
        </div>
        <Button icon="pi pi-plus" :label="t('vpn_add_rule')" severity="secondary" text @click="addRule" />
      </div>

      <div v-if="vpn.rules.length === 0" class="text-sm text-surface-500 dark:text-surface-400">
        {{ t('vpn_no_rules') }}
      </div>

      <div v-for="(rule, index) in vpn.rules" :key="vpnRuleViewKey(rule)"
        data-testid="vpn-rule-card"
        class="grid min-w-0 grid-cols-1 gap-3 rounded border border-surface-200 p-3 dark:border-surface-700 md:grid-cols-12">
        <div class="grid min-w-0 grid-cols-1 gap-3 sm:grid-cols-2 md:col-span-10 md:grid-cols-10">
          <div class="min-w-0 sm:col-span-2 md:col-span-4">
            <label :for="`vpn_rule_cidr_${index}`" class="mb-1 block">{{ t('vpn_rule_cidr') }}</label>
            <InputText :id="`vpn_rule_cidr_${index}`" v-model="rule.cidr" class="w-full min-w-0"
              placeholder="0.0.0.0/0" />
          </div>

          <div class="min-w-0 md:col-span-3">
            <label :for="`vpn_rule_action_${index}`" class="mb-1 block">{{ t('vpn_rule_action') }}</label>
            <Select :input-id="`vpn_rule_action_${index}`" v-model="rule.action" :options="actions"
              option-label="label" option-value="value" class="w-full min-w-0" />
          </div>

          <div class="min-w-0 md:col-span-3">
            <label :for="`vpn_rule_priority_${index}`" class="mb-1 block">{{ t('vpn_rule_priority') }}</label>
            <InputNumber :input-id="`vpn_rule_priority_${index}`" v-model="rule.priority" :format="false"
              :min="0" class="w-full min-w-0" />
          </div>

          <div class="flex min-w-0 flex-col gap-2 sm:col-span-2 md:col-span-10">
            <div class="flex min-w-0 flex-wrap items-center justify-between gap-2">
              <div class="flex min-w-0 flex-wrap items-center">
                <label>{{ t('vpn_rule_exits') }}</label>
                <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('vpn_rule_exits_help')" />
              </div>
              <Button icon="pi pi-plus" text severity="secondary" :disabled="rule.action !== 'exit'"
                :aria-label="t('vpn_rule_add_exit')" v-tooltip="t('vpn_rule_add_exit')"
                @click="addRuleExit(rule)" />
            </div>

            <div v-for="(_, exitIndex) in rule.exits" :key="vpnRuleExitKeys(rule)[exitIndex]"
              data-testid="vpn-exit-row"
              class="flex min-w-0 items-center gap-1">
              <InputText v-model="rule.exits[exitIndex]" class="min-w-0 flex-1"
                :placeholder="t('vpn_rule_exit_placeholder')"
                :aria-label="t('vpn_rule_exit_ip', [exitIndex + 1])" :disabled="rule.action !== 'exit'" />
              <Button icon="pi pi-arrow-up" text :disabled="rule.action !== 'exit' || exitIndex === 0"
                :aria-label="t('vpn_rule_move_exit_up')" v-tooltip="t('vpn_rule_move_exit_up')"
                @click="moveRuleExit(rule, exitIndex, -1)" />
              <Button icon="pi pi-arrow-down" text :disabled="rule.action !== 'exit' || exitIndex === rule.exits.length - 1"
                :aria-label="t('vpn_rule_move_exit_down')" v-tooltip="t('vpn_rule_move_exit_down')"
                @click="moveRuleExit(rule, exitIndex, 1)" />
              <Button icon="pi pi-trash" text severity="danger" :disabled="rule.action !== 'exit'"
                :aria-label="t('vpn_rule_remove_exit')" v-tooltip="t('vpn_rule_remove_exit')"
                @click="removeRuleExit(rule, exitIndex)" />
            </div>
          </div>
        </div>

        <div class="flex min-w-0 flex-wrap items-end gap-1 md:col-span-2 md:justify-end">
          <Button icon="pi pi-arrow-up" text :disabled="index === 0" :aria-label="t('vpn_rule_move_up')"
            v-tooltip="t('vpn_rule_move_up')" @click="moveRule(index, -1)" />
          <Button icon="pi pi-arrow-down" text :disabled="index === vpn.rules.length - 1"
            :aria-label="t('vpn_rule_move_down')" v-tooltip="t('vpn_rule_move_down')"
            @click="moveRule(index, 1)" />
          <Button icon="pi pi-trash" text severity="danger" :aria-label="t('vpn_rule_remove')"
            v-tooltip="t('vpn_rule_remove')" @click="removeRule(index)" />
        </div>
      </div>
    </div>
  </div>
</template>
