<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { Button, Checkbox, Message, SelectButton } from 'primevue'
import { useI18n } from 'vue-i18n'
import Config from './Config.vue'
import type { NetworkConfig } from '../types/network'
import { ConfigFilePermission } from '../modules/api'
import { assertPatchCapabilities } from '../modules/capabilities'
import {
  buildLocalConfigPatch, changedConfigFields, cloneEditableConfig, localConfigEditable,
  localConfigInstanceId, LocalConfigApplyMode, patchStatusName,
  type LocalConfigClient, type LocalConfigEntry, type LocalConfigMachine,
  type LocalConfigPatchResult, type LocalConfigSnapshot,
} from '../modules/localConfigPatch'

const props = defineProps<{ client: LocalConfigClient }>()
const { t } = useI18n()
const machines = ref<LocalConfigMachine[]>([])
const selected = ref<string[]>([])
const loading = ref(false)
const submitting = ref(false)
const error = ref('')
const baseline = ref<NetworkConfig>()
const edited = ref<NetworkConfig>()
const targets = ref<Target[]>([])
const selectedFields = ref<string[]>([])
const applyMode = ref(LocalConfigApplyMode.SaveAndApply)
const results = ref<{ key: string; label: string; status: string; message: string }[]>([])
const pendingDelete = ref('')
const mutating = ref('')
let generation = 0
let timer: ReturnType<typeof setInterval> | undefined

interface Target { key: string; machine: LocalConfigMachine; entry: LocalConfigEntry }
const rows = computed<Target[]>(() => machines.value.flatMap(machine => machine.entries.map(entry => ({
  key: `${machine.machine_id}:${entry.entry_key}`, machine, entry,
}))))
const changedFields = computed(() => baseline.value && edited.value ? changedConfigFields(baseline.value, edited.value) : [])
const dirty = computed(() => changedFields.value.length > 0)
const editorCapabilities = computed(() => [...new Set(targets.value.flatMap(target => target.machine.capabilities))])
const modes = computed(() => [
  { label: t('web.local_configs.save_apply'), value: LocalConfigApplyMode.SaveAndApply },
  { label: t('web.local_configs.persist_only'), value: LocalConfigApplyMode.PersistOnly },
])

function describeError(cause: any): string {
  return cause?.response?.data?.message ?? cause?.message ?? String(cause)
}

function clearEditor(): void {
  baseline.value = undefined
  edited.value = undefined
  targets.value = []
  selectedFields.value = []
}

watch(changedFields, (fields, previous) => {
  const alreadyChanged = new Set(previous ?? [])
  selectedFields.value = fields.filter(field => selectedFields.value.includes(field) || !alreadyChanged.has(field))
})

async function load(observe = false): Promise<void> {
  if (loading.value) return
  const requestGeneration = generation
  const client = props.client
  const scope = client.scope
  const current = () => requestGeneration === generation && client === props.client && scope === client.scope
  loading.value = true
  try {
    const listed = await client.list()
    if (!current()) return
    if (observe) {
      const pending = listed.filter(machine => machine.online)
      await Promise.all(Array.from({ length: Math.min(4, pending.length) }, async () => {
        while (pending.length && current()) {
          const machine = pending.shift()!
          try {
            Object.assign(machine, await client.observe(machine.machine_id))
          } catch {
            machine.stale = true
          }
        }
      }))
    }
    if (!current()) return
    machines.value = listed
    error.value = ''
    if (!edited.value) selected.value = selected.value.filter(key => rows.value.some(row => row.key === key))
  } catch (cause) {
    if (current()) {
      error.value = describeError(cause)
      machines.value = machines.value.map(machine => ({ ...machine, stale: true }))
    }
  } finally {
    if (requestGeneration === generation) loading.value = false
  }
}

function beginEdit(): void {
  const chosen = rows.value.filter(row => selected.value.includes(row.key) && localConfigEditable(row.machine, row.entry))
  if (!chosen.length) return
  targets.value = chosen.map(target => ({ ...target, entry: { ...target.entry } }))
  baseline.value = cloneEditableConfig(chosen[0].entry.config!)
  edited.value = cloneEditableConfig(chosen[0].entry.config!)
  results.value = []
  selectedFields.value = []
}

/** Refresh display/liveness without changing a dirty form or its CAS tokens. */
async function rereadRevisions(): Promise<void> {
  const requestGeneration = generation
  await load(true)
  if (requestGeneration !== generation) return
  targets.value = targets.value.map(target => {
    const fresh = rows.value.find(row => row.key === target.key)
    return fresh ? { ...fresh, entry: { ...fresh.entry } } : target
  })
}

async function submit(): Promise<void> {
  if (submitting.value || !edited.value || !selectedFields.value.length) return
  const client = props.client
  const scope = client.scope
  const requestGeneration = generation
  const current = () => requestGeneration === generation && client === props.client && scope === client.scope
  const form = cloneEditableConfig(edited.value)
  const fields = [...selectedFields.value]
  const mode = applyMode.value
  const pending = [...targets.value]
  const unsuccessful: Target[] = []
  submitting.value = true
  results.value = []
  await Promise.all(Array.from({ length: Math.min(4, pending.length) }, async () => {
    while (pending.length && current()) {
      const target = pending.shift()!
      const label = `${target.machine.hostname || target.machine.machine_id} / ${target.entry.network_name || target.entry.entry_key}`
      try {
        const live = rows.value.find(row => row.key === target.key)
        if (!live || !localConfigEditable(live.machine, live.entry)) throw new Error(t('web.local_configs.unavailable'))
        const supported: string[] = []
        const skipped: string[] = []
        for (const field of fields) {
          try { assertPatchCapabilities(form, [field], live.machine.capabilities); supported.push(field) }
          catch { skipped.push(field) }
        }
        if (!supported.length) throw new Error(t('web.local_configs.unsupported_fields', { fields: skipped.map(field => t(field)).join(', ') }))
        const response = await client.patch(target.machine.machine_id,
          buildLocalConfigPatch(target.entry, form, supported, live.machine.capabilities, mode))
        if (!current()) return
        const outcome = response as LocalConfigPatchResult
        const status = patchStatusName(outcome.status)
        if (status !== 'success') throw new Error(outcome.message || t(`web.local_configs.${status}`))
        const snapshot = 'entries' in response ? response as LocalConfigSnapshot : outcome.snapshot
        if (snapshot) Object.assign(live.machine, snapshot)
        else if (outcome.entry) {
          const index = live.machine.entries.findIndex(entry => entry.entry_key === outcome.entry!.entry_key)
          if (index >= 0) live.machine.entries[index] = outcome.entry
        }
        const message = skipped.length ? t('web.local_configs.unsupported_fields', { fields: skipped.map(field => t(field)).join(', ') }) : ''
        results.value.push({ key: target.key, label, status: skipped.length ? 'partial' : 'success', message })
        if (skipped.length) {
          const fresh = rows.value.find(row => row.key === target.key)
          unsuccessful.push(fresh ? { ...fresh, entry: { ...fresh.entry } } : target)
        }
      } catch (cause) {
        if (!current()) return
        unsuccessful.push(target)
        results.value.push({ key: target.key, label, status: 'failed', message: describeError(cause) })
      }
    }
  }))
  if (!current()) return
  submitting.value = false
  if (unsuccessful.length) targets.value = unsuccessful
  else { clearEditor(); selected.value = [] }
  await load()
}

async function mutate(row: Target, action: 'toggle' | 'remove'): Promise<void> {
  if (mutating.value || edited.value || !localConfigEditable(row.machine, row.entry)) return
  if (action === 'remove' && !ConfigFilePermission.isDeletable(row.entry.config_permission)) return
  const client = props.client
  const scope = client.scope
  const requestGeneration = generation
  const current = () => requestGeneration === generation && client === props.client && scope === client.scope
  const instanceId = localConfigInstanceId(row.entry)
  const revision = row.entry.revision
  mutating.value = row.key
  pendingDelete.value = ''
  try {
    const response = action === 'toggle'
      ? await client.setEnabled!(row.machine.machine_id, instanceId, revision, !row.entry.enabled)
      : await client.remove!(row.machine.machine_id, instanceId, revision)
    if (!current()) return
    const outcome = response as LocalConfigPatchResult
    const status = patchStatusName(outcome.status)
    if (status !== 'success') throw new Error(outcome.message || t(`web.local_configs.${status}`))
    const snapshot = 'entries' in response ? response as LocalConfigSnapshot : outcome.snapshot
    if (snapshot) Object.assign(row.machine, snapshot)
    else if (action === 'remove') row.machine.entries = row.machine.entries.filter(entry => entry.entry_key !== row.entry.entry_key)
    else if (outcome.entry) {
      const index = row.machine.entries.findIndex(entry => entry.entry_key === outcome.entry!.entry_key)
      if (index >= 0) row.machine.entries[index] = outcome.entry
    }
    results.value = [{ key: row.key, label: `${row.machine.hostname || row.machine.machine_id} / ${row.entry.network_name}`, status: 'success', message: '' }]
  } catch (cause) {
    if (current()) results.value = [{ key: row.key, label: row.entry.network_name, status: 'failed', message: describeError(cause) }]
  } finally {
    if (current()) mutating.value = ''
  }
  if (current()) await load()
}

watch([() => props.client, () => props.client.scope], () => {
  generation++
  loading.value = false
  submitting.value = false
  pendingDelete.value = ''
  mutating.value = ''
  machines.value = []
  selected.value = []
  results.value = []
  error.value = ''
  applyMode.value = LocalConfigApplyMode.SaveAndApply
  clearEditor()
  void load(true)
})
onMounted(() => { void load(true); timer = setInterval(() => void load(), 5_000) })
onUnmounted(() => { generation++; clearInterval(timer); clearEditor(); machines.value = [] })
</script>

<template>
  <div class="local-config-batch">
    <header class="batch-heading">
      <div><h2>{{ t('web.local_configs.title') }}</h2><p>{{ t('web.local_configs.description') }}</p></div>
      <Button icon="pi pi-refresh" :label="t('web.console.refresh')" severity="secondary" outlined :loading="loading" @click="load(true)" />
    </header>
    <Message v-if="error" severity="warn" :closable="false">{{ error }}</Message>
    <div class="config-table-wrap">
      <table class="config-table">
        <thead><tr><th>{{ t('web.local_configs.select') }}</th><th>{{ t('web.local_configs.device') }}</th><th>{{ t('network_name') }}</th><th>{{ t('web.local_configs.state') }}</th><th>{{ t('web.local_configs.actions') }}</th></tr></thead>
        <tbody>
          <tr v-for="row in rows" :key="row.key">
            <td><Checkbox v-model="selected" :value="row.key" :input-id="`local-${row.key}`" :disabled="!!edited || !localConfigEditable(row.machine, row.entry)" /></td>
            <td><label :for="`local-${row.key}`">{{ row.machine.hostname || row.machine.machine_id }}</label><small>{{ row.machine.online ? t('web.local_configs.online') : t('web.local_configs.offline') }}{{ row.machine.stale ? ` / ${t('web.local_configs.stale')}` : '' }}</small></td>
            <td>{{ row.entry.network_name || row.entry.entry_key }}<small>{{ localConfigInstanceId(row.entry) }}</small></td>
            <td>{{ t(`web.local_configs.${row.entry.status}`) }}<small>{{ row.entry.running ? t('web.local_configs.running') : row.entry.enabled ? t('web.local_configs.stopped') : t('web.local_configs.disabled') }}{{ row.entry.pending_apply ? ` / ${t('web.local_configs.pending_apply')}` : '' }}</small></td>
            <td class="row-actions">
              <template v-if="localConfigEditable(row.machine, row.entry)">
                <Button v-if="client.setEnabled" :label="t(row.entry.enabled ? 'web.local_configs.disable' : 'web.local_configs.enable')" size="small" severity="secondary" outlined :disabled="!!edited || !!mutating" @click="mutate(row, 'toggle')" />
                <Button v-if="client.remove && ConfigFilePermission.isDeletable(row.entry.config_permission) && pendingDelete !== row.key" :label="t('web.local_configs.delete')" size="small" severity="danger" text :disabled="!!edited || !!mutating" @click="pendingDelete = row.key" />
                <template v-if="pendingDelete === row.key">
                  <Button :label="t('web.local_configs.confirm_delete')" size="small" severity="danger" :disabled="!!mutating" @click="mutate(row, 'remove')" />
                  <Button :label="t('web.common.cancel')" size="small" severity="secondary" text @click="pendingDelete = ''" />
                </template>
              </template>
            </td>
          </tr>
          <tr v-if="!rows.length"><td colspan="5">{{ t('web.local_configs.empty') }}</td></tr>
        </tbody>
      </table>
    </div>
    <Button v-if="!edited" icon="pi pi-pencil" :label="t('web.local_configs.edit_selected', { count: selected.length })" :disabled="!selected.length" @click="beginEdit" />
    <section v-if="edited" class="batch-editor">
      <div class="editor-actions">
        <strong>{{ t('web.local_configs.editing', { count: targets.length }) }}</strong>
        <Button :label="t('web.local_configs.reread')" severity="secondary" outlined :disabled="submitting || loading" :loading="loading" @click="rereadRevisions" />
        <Button :label="t('web.common.cancel')" severity="secondary" text :disabled="submitting" @click="clearEditor" />
      </div>
      <Message v-if="dirty" severity="info" :closable="false">{{ t('web.local_configs.dirty_preserved') }}</Message>
      <SelectButton v-model="applyMode" :options="modes" option-label="label" option-value="value" :allow-empty="false" :disabled="submitting" />
      <div class="selected-fields">
        <p>{{ t('web.local_configs.fields') }}</p>
        <label v-for="field in changedFields" :key="field"><Checkbox v-model="selectedFields" :value="field" :disabled="submitting" />{{ t(field) }}</label>
        <small v-if="!changedFields.length">{{ t('web.local_configs.no_changes') }}</small>
      </div>
      <Config v-model:cur-network="edited" :runtime-capabilities="editorCapabilities" :edit-vpn-portal-clients="true"
        :config-invalid="submitting || !selectedFields.length" :action-label="t(applyMode === 1 ? 'web.local_configs.persist_only' : 'web.local_configs.save_apply')" @run-network="submit" />
    </section>
    <ul v-if="results.length" class="batch-results" aria-live="polite">
      <li v-for="result in results" :key="result.key" :class="`result-${result.status}`"><strong>{{ result.label }}</strong>: {{ t(`web.local_configs.${result.status}`) }}<span v-if="result.message"> — {{ result.message }}</span></li>
    </ul>
  </div>
</template>

<style scoped>
.local-config-batch { display: flex; flex-direction: column; gap: 18px; min-width: 0; }
.batch-heading, .editor-actions { display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
.batch-heading h2 { font-size: 1.2rem; margin: 0; }
.batch-heading p { margin: 8px 0 0; color: var(--p-text-muted-color); font-size: 13px; }
.config-table-wrap { overflow-x: auto; border: 1px solid var(--p-content-border-color); border-radius: 8px; }
.config-table { width: 100%; text-align: left; border-collapse: collapse; }
.config-table th, .config-table td { padding: 12px; border-bottom: 1px solid var(--p-content-border-color); }
.config-table th { font-size: 12px; color: var(--p-text-muted-color); }
.config-table small { display: block; margin-top: 4px; color: var(--p-text-muted-color); font-size: 11px; }
.row-actions { min-width: 180px; }
.row-actions :deep(button) { margin: 2px; }
.batch-editor { display: flex; flex-direction: column; gap: 16px; }
.selected-fields { display: flex; gap: 12px; flex-wrap: wrap; align-items: center; }
.selected-fields p { flex-basis: 100%; margin: 0; }
.selected-fields label { display: inline-flex; gap: 6px; align-items: center; font-size: 13px; }
.batch-results { padding-left: 20px; font-size: 13px; }
.batch-results li { padding: 6px 0; }
.result-failed, .result-partial { color: var(--p-orange-600); }
.result-success { color: var(--p-green-600); }
</style>
