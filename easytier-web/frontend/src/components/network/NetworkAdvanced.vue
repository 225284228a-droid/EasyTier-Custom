<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { Button, Message, Skeleton, Tag, useToast } from 'primevue';
import { useI18n } from 'vue-i18n';
import type { MeshMember, MeshSyncResult } from '../../modules/api';
import { failureMessage } from '../../modules/networkDiscovery';
import { failedResults, resultSeverity, useMeshNetwork } from '../../modules/meshNetworkContext';
import { advancedDistribution, touchedSettings } from '../../modules/meshAdvanced';
import AdvancedFieldsEditor from './AdvancedFieldsEditor.vue';

const { t } = useI18n();
const toast = useToast();
const { api, networkName, network, reload } = useMeshNetwork();

// What each permanent member held when the console last saw it.
const members = ref<MeshMember[]>();
const load = async () => {
    try {
        members.value = (await api.list_mesh_members())
            .filter(member => member.network_name === networkName.value && !member.credential);
    } catch (error) {
        members.value = [];
        toast.add({ severity: 'error', summary: t('web.mesh_network.section_advanced'), detail: failureMessage(error), life: 5000 });
    }
};
onMounted(load);
// An online device says which settings it can store; batch edits skip the rest.
const capabilitiesOf = (deviceId: string) => {
    const device = network.value?.members.find(member => member.device.machine_id === deviceId)?.device;
    return device?.online ? device.runtime_capabilities ?? [] : undefined;
};
const distribution = computed(() => advancedDistribution((members.value ?? []).filter(member => !member.locked)
    .map(member => ({ values: member.advanced ?? {}, capabilities: capabilitiesOf(member.device_id) }))));
const lockedCount = computed(() => members.value?.filter(member => member.locked).length ?? 0);
const targetCount = computed(() => (members.value?.length ?? 0) - lockedCount.value);

const edits = ref<Record<string, any>>({});
const change = computed(() => touchedSettings(edits.value));
const applying = ref(false);
const results = ref<MeshSyncResult[]>([]);
const memberName = (result: MeshSyncResult) => {
    const member = network.value?.members.find(member => member.key === `${result.device_id}/${result.instance_id}`);
    return member ? member.device.alias || member.device.hostname || member.device.machine_id : result.device_id;
};
const apply = async () => {
    if (!Object.keys(change.value).length) return;
    applying.value = true;
    try {
        results.value = await api.apply_mesh_advanced(networkName.value, change.value);
        const failed = failedResults(results.value).length;
        toast.add({
            severity: failed ? 'warn' : 'success',
            summary: t('web.mesh_network.section_advanced'),
            detail: t('web.mesh_network.batch_done', { ok: results.value.length - failed, failed }),
            life: 3000,
        });
        if (!failed) edits.value = {};
        await Promise.all([load(), reload()]);
    } catch (error) {
        toast.add({ severity: 'error', summary: t('web.mesh_network.section_advanced'), detail: failureMessage(error), life: 5000 });
    } finally {
        applying.value = false;
    }
};
</script>

<template>
    <section class="console-panel">
        <header class="section-heading advanced-heading">
            <h2>{{ t('web.mesh_network.section_advanced') }}</h2>
            <div class="flex items-center flex-wrap gap-2 ml-auto">
                <Tag v-if="lockedCount" icon="pi pi-lock" severity="warn" :value="t('web.mesh_network.locked_skipped', { count: lockedCount })" />
                <Button :label="t('web.mesh_network.discard')" icon="pi pi-undo" text severity="secondary" size="small"
                    :disabled="!Object.keys(change).length" @click="edits = {}" />
                <Button :label="t('web.mesh_network.apply_advanced', { count: Object.keys(change).length, members: targetCount })" icon="pi pi-check"
                    size="small" :loading="applying" :disabled="!Object.keys(change).length" @click="apply" />
            </div>
        </header>
        <p class="section-hint">{{ t('web.mesh_network.advanced_hint') }}</p>
        <div v-if="results.length" class="sync-results">
            <h3>{{ t('web.mesh_network.batch_result') }}</h3>
            <div v-for="result in results" :key="`${result.device_id}/${result.instance_id}`" class="sync-result">
                <span class="preview-name">{{ memberName(result) }}</span>
                <Tag :value="t(`web.mesh_network.result_${result.status}`)" :severity="resultSeverity(result.status)" />
                <span v-if="result.message" class="preview-secondary">{{ result.message }}</span>
            </div>
        </div>
        <div class="advanced-body">
            <div v-if="members === undefined" class="loading-rows"><Skeleton v-for="i in 4" :key="i" height="2rem" /></div>
            <template v-else>
                <Message v-if="!members.length" severity="info" :closable="false">{{ t('web.mesh_network.no_members') }}</Message>
                <AdvancedFieldsEditor v-model="edits" mode="change" :distribution="distribution" :disabled="applying" />
            </template>
        </div>
    </section>
</template>

<style scoped>
.advanced-heading { flex-wrap: wrap; }
.section-hint { margin: 0; padding: 0 18px 4px; font-size: 13px; color: var(--p-text-muted-color); }
.advanced-body { padding: 12px 18px 20px; display: flex; flex-direction: column; gap: 12px; }
.sync-results { display: flex; flex-direction: column; gap: 6px; margin: 8px 18px 0; padding: 12px; border: 1px solid var(--console-border); border-radius: 6px; }
.sync-results h3 { margin: 0 0 4px; font-size: 14px; }
.sync-result { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; font-size: 13px; }
</style>
