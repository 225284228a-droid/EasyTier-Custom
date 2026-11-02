<script setup lang="ts">
import { computed, defineAsyncComponent, onMounted, ref, toRaw, watch } from 'vue';
import { Button, Column, ConfirmDialog, DataTable, Dialog, Message, ProgressSpinner, Skeleton, Tag, useConfirm, useToast } from 'primevue';
import { useRoute, useRouter } from 'vue-router';
import { useI18n } from 'vue-i18n';
import { Api as LibApi, Config, ConfigStores, LocalConfigs, NetworkTypes } from 'easytier-frontend-lib';
import ApiClient, { type MeshCoreChange, type MeshSyncResult } from '../modules/api';
import { useMeshTopology } from '../modules/useMeshTopology';
import { networkNameIdentity } from '../modules/networkTopology';
import {
    deviceConfigStore, failureMessage, mergeConfigFields, PER_DEVICE_FIELDS, useNetworkDiscovery, variedFields, type NetworkMember,
} from '../modules/networkDiscovery';

const NetworkTopologyGlobe = defineAsyncComponent(() => import('./NetworkTopologyGlobe.vue'));

const props = defineProps<{ api: ApiClient; centralEnabled?: boolean }>();
const { t } = useI18n();
const route = useRoute();
const router = useRouter();
const toast = useToast();
const confirm = useConfirm();

const networkName = computed(() => String(route.params.networkName ?? ''));
const discovery = useNetworkDiscovery();
const network = computed(() => discovery.networks.value?.find(entry => entry.name === networkName.value));
const members = computed(() => network.value?.members ?? []);
const deviceName = (member: NetworkMember) => member.device.alias || member.device.hostname || member.device.machine_id;
const editable = (member: NetworkMember) => LibApi.ConfigFilePermission.isEditable(member.permission);
// Only members whose configuration was read now can be edited from here;
// temporary members follow their central network.
const actionable = (member: NetworkMember) => member.online && !member.credential && editable(member);
const selectable = ({ data }: { data: NetworkMember }) => actionable(data);

onMounted(() => void discovery.refresh());

// --- globe: the shared topology, narrowed to this network ---

const { topology, manualRefreshing, error: topologyError, persistenceKey, loadTopology } = useMeshTopology(computed(() => props.api));
const networkTopology = computed(() => {
    const identity = networkNameIdentity(networkName.value);
    const nodes = topology.value.nodes.filter(node => node.networkIdentity === identity);
    const ids = new Set(nodes.map(node => node.id));
    return { nodes, links: topology.value.links.filter(link => ids.has(link.source) && ids.has(link.target)) };
});
const globeKey = computed(() => `${persistenceKey.value}:network:${networkName.value}`);

// --- member selection ---

const selection = ref<NetworkMember[]>([]);
const selectedMembers = computed(() => {
    const keys = new Set(selection.value.map(member => member.key));
    return members.value.filter(member => keys.has(member.key));
});
// The batch editor targets the whole network until the user narrows it.
let selectionSeeded = false;
watch(members, list => {
    if (selectionSeeded || !list.length) return;
    selectionSeeded = true;
    selection.value = list.filter(actionable);
}, { immediate: true });

const openDevice = (member: NetworkMember) => router.push({
    name: 'deviceManagement', params: { deviceId: member.device.machine_id, instanceId: member.instanceId },
});


const busyState = ref(false);
const setEnabled = async (enabled: boolean) => {
    const targets = selectedMembers.value.filter(member => member.running !== enabled && actionable(member));
    if (!targets.length) return;
    busyState.value = true;
    try {
        const results = await Promise.allSettled(targets.map(member =>
            deviceConfigStore(props.api, member.device).then(({ store }) => store.setEnabled(member.instanceId, enabled))));
        const failed = results.flatMap((result, index) => result.status === 'rejected'
            ? [`${deviceName(targets[index])}: ${failureMessage(result.reason)}`] : []);
        toast.add({
            severity: failed.length ? 'warn' : 'success',
            summary: t(enabled ? 'web.mesh_network.batch_start' : 'web.mesh_network.batch_stop'),
            detail: t('web.mesh_network.batch_done', { ok: targets.length - failed.length, failed: failed.length })
                + (failed.length ? `: ${failed.join('; ')}` : ''),
            life: failed.length ? 8000 : 2500,
        });
    } finally {
        busyState.value = false;
        await discovery.refresh();
    }
};

// --- batch editor ---

interface LoadedMember {
    member: NetworkMember;
    store: ConfigStores.ConfigStore;
    capabilities: string[];
    document: ConfigStores.ConfigDocument;
}
interface MemberResult {
    member: NetworkMember;
    status: 'saved' | 'unchanged' | 'pending' | 'conflict' | 'apply_failed' | 'failed';
    message?: string;
}

// Peers must share these to stay connected. The console writes them to
// every member it knows and replays a missed change once to an offline one.
const CORE_FIELDS = ['network_name', 'network_secret', 'secure_mode', 'disable_encryption', 'encryption_algorithm'];

const coreChangeOf = (source: NetworkTypes.NetworkConfig, fields: string[]): MeshCoreChange => {
    const backend = NetworkTypes.toBackendNetworkConfig(source) as Record<string, any>;
    const change: MeshCoreChange = {};
    if (fields.includes('network_name')) change.network_name = String(backend.network_name ?? '').trim();
    if (fields.includes('network_secret')) change.network_secret = String(backend.network_secret ?? '');
    if (fields.includes('secure_mode')) change.secure_mode = !!backend.secure_mode?.enabled;
    if (fields.includes('disable_encryption')) change.disable_encryption = !!backend.disable_encryption;
    if (fields.includes('encryption_algorithm')) change.encryption_algorithm = String(backend.encryption_algorithm ?? '');
    return change;
};

const serverStatus = (result: MeshSyncResult): MemberResult['status'] => result.status;

const batchVisible = ref(false);
const batchLoading = ref(false);
const batchApplying = ref(false);
const loaded = ref<LoadedMember[]>([]);
const loadFailures = ref<MemberResult[]>([]);
const template = ref<NetworkTypes.NetworkConfig>();
const templateBase = ref<NetworkTypes.NetworkConfig>();
const differing = ref<string[]>([]);
const results = ref<MemberResult[]>([]);
let batchRequest = 0;

const templateCapabilities = computed(() => loaded.value[0]?.capabilities ?? []);
// Form state is reactive; structuredClone only accepts the raw objects.
const cloneConfig = (config: NetworkTypes.NetworkConfig) => structuredClone(toRaw(config));
const templateDevice = computed(() => loaded.value[0] ? deviceName(loaded.value[0].member) : '');

const openBatchEdit = async () => {
    const targets = selectedMembers.value.filter(editable);
    if (!targets.length) return;
    const request = ++batchRequest;
    batchVisible.value = true;
    batchLoading.value = true;
    loaded.value = [];
    loadFailures.value = [];
    results.value = [];
    template.value = undefined;
    const settled = await Promise.allSettled(targets.map(async member => {
        const { store, capabilities } = await deviceConfigStore(props.api, member.device);
        return { member, store, capabilities, document: await store.load(member.instanceId) };
    }));
    if (request !== batchRequest) return;
    loaded.value = settled.flatMap(result => result.status === 'fulfilled' ? [result.value] : []);
    loadFailures.value = settled.flatMap((result, index) => result.status === 'rejected'
        ? [{ member: targets[index], status: 'failed' as const, message: failureMessage(result.reason) }] : []);
    if (loaded.value.length) {
        templateBase.value = cloneConfig(loaded.value[0].document.config);
        template.value = cloneConfig(loaded.value[0].document.config);
        differing.value = variedFields(loaded.value.map(entry => entry.document.config));
    }
    batchLoading.value = false;
};

watch(batchVisible, visible => { if (!visible) batchRequest++; });

const applyBatch = async () => {
    if (!template.value || !templateBase.value) return;
    const fields = LocalConfigs.changedConfigFields(templateBase.value, template.value);
    if (!fields.length) {
        toast.add({ severity: 'info', summary: t('web.mesh_network.batch_edit'), detail: t('web.mesh_network.batch_no_changes'), life: 2500 });
        return;
    }
    const perDevice = fields.filter(field => PER_DEVICE_FIELDS.includes(field));
    if (perDevice.length && loaded.value.length > 1) {
        confirm.require({
            header: t('web.mesh_network.batch_per_device_title'),
            message: t('web.mesh_network.batch_per_device_confirm', { fields: perDevice.map(field => t(field)).join(', ') }),
            acceptLabel: t('web.common.confirm'),
            rejectLabel: t('web.common.cancel'),
            accept: () => void writeBatch(fields),
        });
        return;
    }
    await writeBatch(fields);
};

const writeBatch = async (fields: string[]) => {
    const source = template.value!;
    const coreFields = fields.filter(field => CORE_FIELDS.includes(field));
    const otherFields = fields.filter(field => !CORE_FIELDS.includes(field));
    batchApplying.value = true;
    try {
        // Core settings go through the console, which also reaches offline
        // members. Without central management they are written here instead.
        const written = new Map<string, MeshSyncResult>();
        if (coreFields.length && props.centralEnabled) {
            try {
                for (const result of await props.api.apply_mesh_core(networkName.value, coreChangeOf(source, coreFields))) {
                    written.set(`${result.device_id}/${result.instance_id}`, result);
                }
            } catch (error) {
                toast.add({ severity: 'warn', summary: t('web.mesh_network.batch_edit'), detail: failureMessage(error), life: 5000 });
            }
        }
        const settled = await Promise.allSettled(loaded.value.map(async ({ member, store, document }) => {
            // Official nodes are not reachable by the console's core writes.
            const clientFields = written.has(member.key) ? otherFields : fields;
            if (!clientFields.length) return document;
            // A core write by the console gave the file a new revision.
            const base = written.get(member.key)?.status === 'saved' ? await store.load(member.instanceId) : document;
            return store.save(base, mergeConfigFields(base.config, source, clientFields), {
                running: member.running,
                persist: LibApi.ConfigFilePermission.isRemoveSaveable(member.permission),
            });
        }));
        const loadedKeys = new Set(loaded.value.map(entry => entry.member.key));
        results.value = [
            ...settled.map((result, index): MemberResult => {
                const member = loaded.value[index].member;
                const core = written.get(member.key);
                if (core && core.status !== 'saved' && core.status !== 'unchanged') {
                    return { member, status: serverStatus(core), message: core.message };
                }
                if (result.status === 'fulfilled') {
                    const clientWrote = (written.has(member.key) ? otherFields : fields).length > 0;
                    return { member, status: clientWrote || core?.status === 'saved' ? 'saved' : 'unchanged' };
                }
                const reason = result.reason;
                if (reason instanceof ConfigStores.ConfigConflictError) return { member, status: 'conflict' };
                if (reason instanceof ConfigStores.ConfigApplyError) return { member, status: 'apply_failed', message: reason.message };
                return { member, status: 'failed', message: failureMessage(reason) };
            }),
            // Members only the console reached: offline ones and unselected ones.
            ...[...written.values()]
                .filter(result => !loadedKeys.has(`${result.device_id}/${result.instance_id}`))
                .map((result): MemberResult => ({
                    member: members.value.find(member => member.key === `${result.device_id}/${result.instance_id}`)
                        ?? { key: `${result.device_id}/${result.instance_id}`, device: { machine_id: result.device_id, hostname: result.device_id } as any, instanceId: result.instance_id, running: false, permission: 0, online: false },
                    status: serverStatus(result),
                    message: result.message,
                })),
        ];
        // Later edits start from what each member stores now.
        loaded.value = loaded.value.map((entry, index) => {
            const result = settled[index];
            return result.status === 'fulfilled' ? { ...entry, document: result.value } : entry;
        });
        templateBase.value = cloneConfig(source);
        differing.value = variedFields(loaded.value.map(entry => entry.document.config));
        const failed = results.value.filter(result => !['saved', 'unchanged', 'pending'].includes(result.status)).length;
        toast.add({
            severity: failed ? 'warn' : 'success',
            summary: t('web.mesh_network.batch_result'),
            detail: t('web.mesh_network.batch_done', { ok: results.value.length - failed, failed }),
            life: 3000,
        });
        await discovery.refresh();
        const renamed = String(source.network_name ?? '').trim();
        if (fields.includes('network_name') && renamed && renamed !== networkName.value && !failed) {
            toast.add({ severity: 'info', summary: t('web.mesh_network.batch_edit'), detail: t('web.mesh_network.renamed', { name: renamed }), life: 3000 });
            batchVisible.value = false;
            router.replace({ name: 'meshNetwork', params: { networkName: renamed } });
        }
    } finally {
        batchApplying.value = false;
    }
};

const resultSeverity = (status: MemberResult['status']) => status === 'saved' || status === 'unchanged' ? 'success'
    : status === 'pending' ? 'info' : status === 'failed' ? 'danger' : 'warn';
const resultLabel = (status: MemberResult['status']) => t(`web.mesh_network.result_${status}`);
</script>

<template>
    <div class="console-page">
        <ConfirmDialog />
        <header class="page-heading">
            <div class="min-w-0">
                <RouterLink :to="{ name: 'networkList' }" class="entity-link text-xs"><i class="pi pi-arrow-left" aria-hidden="true"></i>{{ t('web.main.network_list') }}</RouterLink>
                <h1 class="mesh-title"><i class="pi pi-globe" aria-hidden="true"></i><span>{{ network?.label ?? networkName }}</span>
                    <span v-if="network && network.label !== networkName" class="mono-value muted">{{ networkName }}</span>
                    <Tag v-if="network?.central" :value="t('web.mesh_network.central')" severity="info" /></h1>
            </div>
            <div class="flex flex-wrap gap-2">
                <RouterLink v-if="network?.central && centralEnabled" :to="{ name: 'networkDetail', params: { networkId: network.central.network_id } }">
                    <Button :label="t('web.mesh_network.open_central')" icon="pi pi-sitemap" severity="secondary" outlined />
                </RouterLink>
                <Button icon="pi pi-refresh" :label="t('web.console.refresh')" severity="secondary" outlined :loading="discovery.refreshing.value" @click="discovery.refresh()" />
            </div>
        </header>
        <Message v-if="network?.central" severity="info" :closable="false">{{ t('web.mesh_network.central_hint') }}</Message>
        <Message v-if="discovery.failedDevices.value" severity="warn" :closable="false">{{ t('web.mesh_network.scan_partial', { count: discovery.failedDevices.value }) }}</Message>

        <section class="console-panel summary-strip">
            <div class="summary-stat"><div class="summary-label"><span>{{ t('web.mesh_network.devices') }}</span><i class="pi pi-server" aria-hidden="true"></i></div>
                <div class="summary-value">{{ new Set(members.map(member => member.device.machine_id)).size }}</div></div>
            <div class="summary-stat"><div class="summary-label"><span>{{ t('web.mesh_network.running') }}</span><i class="pi pi-play" aria-hidden="true"></i></div>
                <div class="summary-value">{{ network?.runningCount ?? 0 }}<span class="summary-total"> / {{ members.length }}</span></div></div>
            <div class="summary-stat"><div class="summary-label"><span>{{ t('web.mesh_network.nodes') }}</span><i class="pi pi-share-alt" aria-hidden="true"></i></div>
                <div class="summary-value">{{ networkTopology.nodes.length }}</div></div>
            <div class="summary-stat"><div class="summary-label"><span>{{ t('web.dashboard.connections') }}</span><i class="pi pi-arrows-h" aria-hidden="true"></i></div>
                <div class="summary-value">{{ networkTopology.links.length }}</div></div>
        </section>

        <section class="console-panel">
            <header class="section-heading mesh-members-heading">
                <h2>{{ t('web.mesh_network.members') }}</h2>
                <div class="mesh-actions">
                    <span v-if="selectedMembers.length" class="table-count">{{ t('web.mesh_network.selected', { count: selectedMembers.length }) }}</span>
                    <Button icon="pi pi-play" :label="t('web.mesh_network.batch_start')" size="small" severity="secondary" outlined
                        :disabled="!selectedMembers.some(member => !member.running && editable(member))" :loading="busyState" @click="setEnabled(true)" />
                    <Button icon="pi pi-stop" :label="t('web.mesh_network.batch_stop')" size="small" severity="secondary" outlined
                        :disabled="!selectedMembers.some(member => member.running && editable(member))" :loading="busyState" @click="setEnabled(false)" />
                    <Button icon="pi pi-pencil" :label="t('web.mesh_network.batch_edit')" size="small"
                        :disabled="!selectedMembers.some(editable)" @click="openBatchEdit" />
                </div>
            </header>
            <div v-if="discovery.networks.value === undefined" class="loading-rows"><Skeleton v-for="i in 3" :key="i" height="2rem" /></div>
            <div v-else-if="!members.length" class="console-empty-state">
                <i class="pi pi-globe" aria-hidden="true"></i>
                <h2>{{ network ? t('web.mesh_network.no_members') : t('web.mesh_network.not_found', { name: networkName }) }}</h2>
            </div>
            <DataTable v-else v-model:selection="selection" :value="members" dataKey="key" class="console-table" size="small" scrollable
                :paginator="members.length > 20" :rows="20" :isDataSelectable="selectable">
                <Column selectionMode="multiple" headerStyle="width: 3rem" />
                <Column :header="t('web.mesh_network.devices')">
                    <template #body="{ data }">
                        <button class="entity-link" @click="openDevice(data)"><i class="pi pi-server entity-icon" aria-hidden="true"></i>
                            <span class="min-w-0"><span class="preview-name">{{ deviceName(data) }}</span>
                                <span class="preview-secondary mono-value block">{{ data.device.public_ip || '—' }}</span></span></button>
                    </template>
                </Column>
                <Column :header="t('web.mesh_network.instance')"><template #body="{ data }"><span class="mono-value" :title="data.instanceId">{{ data.instanceId.slice(0, 8) }}</span></template></Column>
                <Column :header="t('web.mesh_network.status')">
                    <template #body="{ data }">
                        <div class="flex flex-wrap gap-1">
                            <Tag v-if="!data.online" :value="t('web.mesh_network.offline')" severity="secondary" />
                            <Tag v-else :value="data.running ? t('web.mesh_network.running') : t('web.mesh_network.stopped')" :severity="data.running ? 'success' : 'secondary'" />
                            <Tag v-if="data.pending" :value="t('web.mesh_network.pending')" severity="info" v-tooltip.top="t('web.mesh_network.pending_hint')" />
                            <Tag v-if="data.credential" :value="t('web.mesh_network.temporary')" severity="contrast" />
                            <Tag v-if="!editable(data)" :value="t('web.mesh_network.readonly')" severity="warn" />
                        </div>
                    </template>
                </Column>
                <Column field="device.easytier_version" :header="t('web.device.version')" />
                <Column :header="t('web.console.manage')">
                    <template #body="{ data }"><Button icon="pi pi-arrow-up-right" text severity="secondary" v-tooltip="t('web.mesh_network.open_device')" :aria-label="t('web.mesh_network.open_device')" @click="openDevice(data)" /></template>
                </Column>
            </DataTable>
        </section>

        <section class="console-panel dashboard">
            <header class="section-heading"><h2>{{ t('web.mesh_network.topology') }}</h2></header>
            <div class="mesh-globe">
                <p class="mesh-globe-hint">{{ t('web.mesh_network.topology_hint', { name: networkName }) }}</p>
                <p v-if="topologyError" role="status" class="dashboard-error">{{ topologyError }}</p>
                <NetworkTopologyGlobe :nodes="networkTopology.nodes" :links="networkTopology.links" :loading="manualRefreshing"
                    :persistence-key="globeKey" @refresh="loadTopology(true)" />
            </div>
        </section>

        <Dialog v-model:visible="batchVisible" modal class="console-dialog" :header="t('web.mesh_network.batch_title', { name: networkName })"
            :style="{ width: '72rem', maxWidth: 'calc(100vw - 32px)' }">
            <div v-if="batchLoading" class="flex items-center gap-3 p-4">
                <ProgressSpinner style="width: 1.5rem; height: 1.5rem" />
                <span>{{ t('web.mesh_network.batch_loading', { count: selectedMembers.length }) }}</span>
            </div>
            <div v-else class="flex flex-col gap-3">
                <Message v-if="loadFailures.length" severity="warn" :closable="false">
                    {{ t('web.mesh_network.batch_load_failed', { count: loadFailures.length }) }}
                    <ul class="batch-failures"><li v-for="failure in loadFailures" :key="failure.member.key">{{ deviceName(failure.member) }}: {{ failure.message }}</li></ul>
                </Message>
                <template v-if="template">
                    <Message severity="info" :closable="false">
                        {{ t('web.mesh_network.batch_template', { device: templateDevice }) }}
                        <div class="mt-2">{{ centralEnabled ? t('web.mesh_network.core_hint') : t('web.mesh_network.core_hint_local') }}</div>
                        <div v-if="differing.length" class="batch-varied">
                            <span>{{ t('web.mesh_network.batch_varied') }}</span>
                            <Tag v-for="field in differing" :key="field" :value="t(field)" severity="secondary" />
                        </div>
                    </Message>
                    <div v-if="results.length" class="batch-results">
                        <h3>{{ t('web.mesh_network.batch_result') }}</h3>
                        <div v-for="result in results" :key="result.member.key" class="batch-result">
                            <span class="preview-name">{{ deviceName(result.member) }}</span>
                            <Tag :value="resultLabel(result.status)" :severity="resultSeverity(result.status)" />
                            <span v-if="result.message" class="preview-secondary">{{ result.message }}</span>
                        </div>
                    </div>
                    <Config v-model:cur-network="template" :config-invalid="batchApplying" :runtime-capabilities="templateCapabilities"
                        :action-label="t('web.mesh_network.batch_apply', { count: loaded.length })" @run-network="applyBatch" />
                </template>
            </div>
        </Dialog>
    </div>
</template>

<style scoped>
.mesh-title { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; overflow-wrap: anywhere; }
.summary-total { font-size: 0.5em; color: var(--p-text-muted-color); }
.mesh-members-heading { flex-wrap: wrap; }
.mesh-actions { display: flex; align-items: center; flex-wrap: wrap; gap: 8px; margin-left: auto; }
.mesh-globe { min-width: 0; padding: 16px 20px 20px; }
.mesh-globe-hint { margin: 0 0 12px; font-size: 13px; color: var(--p-text-muted-color); }
.dashboard-error { margin: 0 0 16px; font-size: 13px; color: var(--p-orange-600); overflow-wrap: anywhere; }
.batch-varied { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; margin-top: 8px; }
.batch-failures { margin: 6px 0 0; padding-left: 18px; }
.batch-results { display: flex; flex-direction: column; gap: 6px; padding: 12px; border: 1px solid var(--console-border); border-radius: 6px; }
.batch-results h3 { margin: 0 0 4px; font-size: 14px; }
.batch-result { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; font-size: 13px; }
</style>
