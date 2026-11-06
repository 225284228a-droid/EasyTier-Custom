<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { Button, Checkbox, Column, DataTable, Dialog, Message, SelectButton, Skeleton, Tag, useToast } from 'primevue';
import { useI18n } from 'vue-i18n';
import { Api as LibApi, Utils } from 'easytier-frontend-lib';
import type { MeshJoinResult } from '../../modules/api';
import { deviceConfigStore, failureMessage, retryDeviceWrite, type NetworkMember } from '../../modules/networkDiscovery';
import { deviceName, failedResults, resultSeverity, useMeshNetwork } from '../../modules/meshNetworkContext';
import { useDevicePopup } from '../../modules/devicePopup';
import EntityName from '../EntityName.vue';

const { t } = useI18n();
const toast = useToast();
const { api, networkName, network, centralEnabled, reload } = useMeshNetwork();
const devicePopup = useDevicePopup();

const members = computed(() => network.value?.members ?? []);
const editable = (member: NetworkMember) => LibApi.ConfigFilePermission.isEditable(member.permission);
// Batch start and stop reach members whose configuration was read now;
// temporary members follow their central record. A lock only keeps batch
// edits off the settings, so locked members start and stop like the rest.
const actionable = (member: NetworkMember) => member.online && !member.credential && editable(member);
const selectable = ({ data }: { data: NetworkMember }) => actionable(data);

const viewMode = ref<'table' | 'card'>(localStorage.getItem('meshMembers.viewMode') === 'table' ? 'table' : 'card');
watch(viewMode, mode => localStorage.setItem('meshMembers.viewMode', mode));
const viewModeOptions = [
    { value: 'card', icon: 'pi pi-th-large' },
    { value: 'table', icon: 'pi pi-table' },
];
const viewModeLabel = (value: string) => t(value === 'card' ? 'web.console.view_card' : 'web.console.view_table');

const selectedKeys = ref<string[]>([]);
const selectedMembers = computed(() => members.value.filter(member => selectedKeys.value.includes(member.key)));
const selectableMembers = computed(() => members.value.filter(actionable));
const allSelected = computed(() => selectableMembers.value.length > 0
    && selectableMembers.value.every(member => selectedKeys.value.includes(member.key)));
const toggleAll = (checked: boolean) => {
    selectedKeys.value = checked ? selectableMembers.value.map(member => member.key) : [];
};
const tableSelection = computed({
    get: () => selectedMembers.value,
    set: (rows: NetworkMember[]) => { selectedKeys.value = rows.map(member => member.key); },
});

const busy = ref(false);
const setEnabled = async (enabled: boolean) => {
    const targets = selectedMembers.value.filter(member => actionable(member) && member.running !== enabled);
    if (!targets.length) return;
    busy.value = true;
    try {
        // Devices are reached at once; each retries over an unsteady link.
        const results = await Promise.allSettled(targets.map(member => retryDeviceWrite(() =>
            deviceConfigStore(api, member.device).then(({ store }) => store.setEnabled(member.instanceId, enabled)))));
        const failed = results.flatMap((result, index) => result.status === 'rejected'
            ? [`${memberName(targets[index])}: ${failureMessage(result.reason)}`] : []);
        toast.add({
            severity: failed.length ? 'warn' : 'success',
            summary: t(enabled ? 'web.mesh_network.batch_start' : 'web.mesh_network.batch_stop'),
            detail: t('web.mesh_network.batch_done', { ok: targets.length - failed.length, failed: failed.length })
                + (failed.length ? `: ${failed.join('; ')}` : ''),
            life: failed.length ? 8000 : 2500,
        });
    } finally {
        busy.value = false;
        await reload();
    }
};

const openStatus = (member: NetworkMember) => devicePopup.open(member.device.machine_id, member.instanceId);
const openEditor = (member: NetworkMember) => devicePopup.open(member.device.machine_id, member.instanceId, true);

// Running networks report their address and why they failed, read in one request.
interface Runtime { address?: string; error?: string }
const runtimes = ref<Record<string, Runtime>>({});
const runningDevices = computed(() => [...new Set(members.value
    .filter(member => member.online && member.running).map(member => member.device.machine_id))].sort().join(','));
watch(runningDevices, async devices => {
    if (!devices) {
        runtimes.value = {};
        return;
    }
    try {
        const machines = await api.collect_topology_network_info(devices.split(','));
        if (devices !== runningDevices.value) return;
        const next: Record<string, Runtime> = {};
        for (const [machineId, { info }] of Object.entries(machines)) {
            for (const [instanceId, detail] of Object.entries(info?.info?.map ?? {})) {
                const address = detail?.my_node_info?.virtual_ipv4;
                next[`${machineId}/${instanceId}`] = {
                    address: address?.address ? Utils.ipv4InetToString(address) : undefined,
                    error: detail?.error_msg || undefined,
                };
            }
        }
        runtimes.value = next;
    } catch (error) {
        console.debug('Failed to read running networks', error);
    }
}, { immediate: true });
const runtime = (member: NetworkMember): Runtime => member.running ? runtimes.value[member.key] ?? {} : {};
const virtualIp = (member: NetworkMember) => runtime(member).address;
// A member goes by the device's name in the device list.
const memberName = deviceName;
const stateOf = (member: NetworkMember) => !member.online ? 'offline'
    : !member.running ? 'stopped' : runtime(member).error ? 'failed' : 'running';
const stateLabel = (member: NetworkMember) => {
    const label = t(`web.mesh_network.${stateOf(member)}`);
    const error = runtime(member).error;
    return error ? `${label}: ${error}` : label;
};
const lastSeen = (member: NetworkMember) => member.device.last_seen?.replace('T', ' ').slice(0, 16) || '—';

// --- adding devices: they join with the core settings and the preset ---

const joinVisible = ref(false);
const joining = ref(false);
const candidates = ref<Utils.DeviceInfo[]>();
const joinSelected = ref<string[]>([]);
const joinResults = ref<MeshJoinResult[]>([]);
const openJoin = async () => {
    joinVisible.value = true;
    joinSelected.value = [];
    joinResults.value = [];
    candidates.value = undefined;
    const memberDevices = new Set(members.value.map(member => member.device.machine_id));
    try {
        candidates.value = (await api.list_machines()).map(Utils.buildDeviceInfo)
            .filter(device => device.machine_id && device.online !== false && !memberDevices.has(device.machine_id))
            .sort((a, b) => (a.alias || a.hostname || '').localeCompare(b.alias || b.hostname || ''));
    } catch (error) {
        candidates.value = [];
        toast.add({ severity: 'error', summary: t('web.mesh_network.add_device'), detail: failureMessage(error), life: 5000 });
    }
};
const join = async () => {
    joining.value = true;
    try {
        joinResults.value = await api.join_mesh_devices(networkName.value, joinSelected.value);
        const failed = failedResults(joinResults.value).length;
        toast.add({
            severity: failed ? 'warn' : 'success',
            summary: t('web.mesh_network.add_device'),
            detail: t('web.mesh_network.batch_done', { ok: joinResults.value.length - failed, failed }),
            life: 3000,
        });
        if (!failed) joinVisible.value = false;
        await reload();
    } catch (error) {
        toast.add({ severity: 'error', summary: t('web.mesh_network.add_device'), detail: failureMessage(error), life: 5000 });
    } finally {
        joining.value = false;
    }
};
const candidateName = (deviceId: string) => {
    const device = candidates.value?.find(candidate => candidate.machine_id === deviceId);
    return device ? device.alias || device.hostname || device.machine_id : deviceId;
};
</script>

<template>
    <section class="console-panel">
        <header class="section-heading members-heading">
            <h2>{{ t('web.mesh_network.members') }}</h2>
            <div class="members-actions">
                <SelectButton v-model="viewMode" :options="viewModeOptions" option-value="value" :allow-empty="false" size="small" class="view-toggle">
                    <template #option="{ option }">
                        <i :class="option.icon" v-tooltip.top="viewModeLabel(option.value)" :aria-label="viewModeLabel(option.value)"></i>
                    </template>
                </SelectButton>
                <div class="flex items-center gap-2">
                    <Checkbox input-id="members-select-all" :model-value="allSelected" binary :disabled="!selectableMembers.length"
                        @update:model-value="toggleAll" />
                    <label for="members-select-all" class="text-sm">{{ t('web.mesh_network.select_all') }}</label>
                </div>
                <span v-if="selectedMembers.length" class="table-count">{{ t('web.mesh_network.selected', { count: selectedMembers.length }) }}</span>
                <Button icon="pi pi-play" :label="t('web.mesh_network.batch_start')" size="small" severity="secondary" outlined
                    :disabled="!selectedMembers.some(member => !member.running)" :loading="busy" @click="setEnabled(true)" />
                <Button icon="pi pi-stop" :label="t('web.mesh_network.batch_stop')" size="small" severity="secondary" outlined
                    :disabled="!selectedMembers.some(member => member.running)" :loading="busy" @click="setEnabled(false)" />
                <Button v-if="centralEnabled" icon="pi pi-plus" :label="t('web.mesh_network.add_device')" size="small" @click="openJoin" />
            </div>
        </header>
        <p class="members-hint">{{ t('web.mesh_network.members_hint') }}</p>
        <div v-if="!network && members.length === 0" class="loading-rows"><Skeleton v-for="i in 3" :key="i" height="2rem" /></div>
        <div v-else-if="!members.length" class="console-empty-state">
            <i class="pi pi-server" aria-hidden="true"></i>
            <h2>{{ t('web.mesh_network.no_members') }}</h2>
            <Button v-if="centralEnabled" icon="pi pi-plus" :label="t('web.mesh_network.add_device')" @click="openJoin" />
        </div>
        <div v-else-if="viewMode === 'card'" class="entity-cards member-cards">
            <article v-for="member in members" :key="member.key" class="entity-card" :class="{ selected: selectedKeys.includes(member.key) }">
                <header class="entity-card-head">
                    <Checkbox v-model="selectedKeys" :value="member.key" :input-id="`member-${member.key}`" :disabled="!actionable(member)"
                        :aria-label="memberName(member)" />
                    <span class="status-dot" :class="stateOf(member)" role="img" :aria-label="stateLabel(member)" v-tooltip.top="stateLabel(member)"></span>
                    <EntityName :name="memberName(member)" />
                </header>
                <dl class="entity-facts">
                    <div><dt>{{ t('web.mesh_network.virtual_ip') }}</dt><dd class="mono-value">{{ virtualIp(member) || '—' }}</dd></div>
                    <div><dt>{{ t('web.device.last_seen') }}</dt><dd>{{ lastSeen(member) }}</dd></div>
                </dl>
                <footer class="entity-card-foot">
                    <div class="entity-flags">
                        <i v-if="member.locked" class="pi pi-lock entity-lock" role="img" :aria-label="t('web.mesh_network.locked')"
                            v-tooltip.top="t('advanced_locked_hint')"></i>
                        <span v-if="runtime(member).error" class="entity-error" :title="runtime(member).error"><i class="pi pi-exclamation-circle" aria-hidden="true"></i>{{ runtime(member).error }}</span>
                        <span v-if="member.pending" class="entity-flag info" v-tooltip.top="t('web.mesh_network.pending_hint')"><i class="pi pi-clock" aria-hidden="true"></i>{{ t('web.mesh_network.pending') }}</span>
                    </div>
                    <div class="entity-actions">
                        <Button icon="pi pi-chart-line" :label="t('web.mesh_network.status')" text size="small" severity="secondary" @click="openStatus(member)" />
                        <Button icon="pi pi-pencil" :label="t('web.console.edit')" :aria-label="t('web.mesh_network.edit_config')" text size="small" severity="secondary" @click="openEditor(member)" />
                    </div>
                </footer>
            </article>
        </div>
        <DataTable v-else v-model:selection="tableSelection" :value="members" data-key="key" class="console-table" size="small" scrollable
            :paginator="members.length > 20" :rows="20" :is-data-selectable="selectable">
            <Column selection-mode="multiple" header-style="width: 3rem" />
            <Column :header="t('web.mesh_network.devices')">
                <template #body="{ data }">
                    <span class="member-cell"><span class="status-dot" :class="stateOf(data)" role="img" :aria-label="stateLabel(data)" v-tooltip.top="stateLabel(data)"></span>
                        <span class="preview-name">{{ memberName(data) }}</span></span>
                </template>
            </Column>
            <Column :header="t('web.mesh_network.virtual_ip')"><template #body="{ data }"><span class="mono-value">{{ virtualIp(data) || '—' }}</span></template></Column>
            <Column :header="t('web.mesh_network.status')">
                <template #body="{ data }">
                    <div class="flex flex-wrap items-center gap-1">
                        <i v-if="data.locked" class="pi pi-lock entity-lock" role="img" :aria-label="t('web.mesh_network.locked')"
                            v-tooltip.top="t('advanced_locked_hint')"></i>
                        <span v-if="runtime(data).error" class="entity-error" :title="runtime(data).error"><i class="pi pi-exclamation-circle" aria-hidden="true"></i>{{ runtime(data).error }}</span>
                        <Tag v-if="data.pending" :value="t('web.mesh_network.pending')" severity="info" v-tooltip.top="t('web.mesh_network.pending_hint')" />
                    </div>
                </template>
            </Column>
            <Column :header="t('web.device.last_seen')"><template #body="{ data }"><span class="muted">{{ lastSeen(data) }}</span></template></Column>
            <Column :header="t('web.console.manage')">
                <template #body="{ data }">
                    <div class="flex items-center gap-1">
                        <Button icon="pi pi-chart-line" :label="t('web.mesh_network.status')" text size="small" severity="secondary" @click="openStatus(data)" />
                        <Button icon="pi pi-pencil" :label="t('web.mesh_network.edit_config')" text size="small" severity="secondary" @click="openEditor(data)" />
                    </div>
                </template>
            </Column>
        </DataTable>
    </section>

    <Dialog v-model:visible="joinVisible" modal class="console-dialog" :header="t('web.mesh_network.add_device')" :style="{ width: '36rem', maxWidth: 'calc(100vw - 32px)' }">
        <div class="flex flex-col gap-3">
            <p class="text-sm muted m-0">{{ t('web.mesh_network.add_device_hint') }}</p>
            <div v-if="candidates === undefined" class="loading-rows"><Skeleton v-for="i in 3" :key="i" height="2rem" /></div>
            <div v-else-if="!candidates.length" class="text-sm muted">{{ t('web.mesh_network.no_candidate_devices') }}</div>
            <div v-else class="device-pick-grid">
                <div v-for="device in candidates" :key="device.machine_id" class="device-pick">
                    <Checkbox v-model="joinSelected" :value="device.machine_id" :input-id="`join-${device.machine_id}`" />
                    <label :for="`join-${device.machine_id}`" class="min-w-0"><span class="preview-name">{{ device.alias || device.hostname || device.machine_id }}</span>
                        <span class="preview-secondary">{{ t('web.console.instances') }} · {{ device.running_network_count }}</span></label>
                </div>
            </div>
            <Message v-for="result in failedResults(joinResults)" :key="result.device_id" :severity="resultSeverity(result.status)" size="small" :closable="false">
                {{ candidateName(result.device_id) }}: {{ result.message ?? t(`web.mesh_network.result_${result.status}`) }}
            </Message>
        </div>
        <template #footer>
            <Button :label="t('web.common.cancel')" icon="pi pi-times" text severity="secondary" @click="joinVisible = false" />
            <Button :label="t('web.mesh_network.add_device_confirm', { count: joinSelected.length })" icon="pi pi-check" :loading="joining"
                :disabled="!joinSelected.length" @click="join" />
        </template>
    </Dialog>
</template>

<style scoped>
.members-heading { flex-wrap: wrap; }
.members-actions { display: flex; align-items: center; flex-wrap: wrap; gap: 8px; margin-left: auto; }
.members-hint { margin: 0; padding: 0 20px 12px; font-size: 13px; color: var(--p-text-muted-color); }
.member-cards { padding-top: 0; }
.member-cell { display: inline-flex; align-items: center; gap: 10px; min-width: 0; font-weight: 550; }
.device-pick-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(min(100%, 160px), 1fr)); gap: 8px; max-height: 18rem; overflow: auto; }
.device-pick { display: flex; align-items: center; gap: 8px; min-width: 0; padding: 8px 10px; border: 1px solid var(--console-border); border-radius: 6px; }
.device-pick label { cursor: pointer; }
.device-pick .preview-name { display: block; font-size: 13px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.device-pick .preview-secondary { display: block; font-size: 11px; }
</style>
