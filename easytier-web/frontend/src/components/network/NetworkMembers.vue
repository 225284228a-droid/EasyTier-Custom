<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { Button, Checkbox, Column, DataTable, Dialog, Message, SelectButton, Skeleton, Tag, useToast } from 'primevue';
import { useI18n } from 'vue-i18n';
import { Api as LibApi, Utils } from 'easytier-frontend-lib';
import type { MeshJoinResult } from '../../modules/api';
import { deviceConfigStore, failureMessage, type NetworkMember } from '../../modules/networkDiscovery';
import { deviceName, failedResults, resultSeverity, useMeshNetwork } from '../../modules/meshNetworkContext';
import { useDevicePopup } from '../../modules/devicePopup';

const { t } = useI18n();
const toast = useToast();
const { api, networkName, network, centralEnabled, reload } = useMeshNetwork();
const devicePopup = useDevicePopup();

const members = computed(() => network.value?.members ?? []);
const editable = (member: NetworkMember) => LibApi.ConfigFilePermission.isEditable(member.permission);
// Batch start and stop reach members whose configuration was read now;
// temporary members follow their central record and locked ones are left alone.
const actionable = (member: NetworkMember) => member.online && !member.credential && editable(member) && !member.locked;
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
        const results = await Promise.allSettled(targets.map(member =>
            deviceConfigStore(api, member.device).then(({ store }) => store.setEnabled(member.instanceId, enabled))));
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
        busy.value = false;
        await reload();
    }
};

const openDevice = (member: NetworkMember) => devicePopup.open(member.device.machine_id, member.instanceId);

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
        <div v-else-if="viewMode === 'card'" class="member-cards">
            <article v-for="member in members" :key="member.key" class="member-card" :class="{ offline: !member.online, selected: selectedKeys.includes(member.key) }">
                <div class="member-card-title">
                    <Checkbox v-model="selectedKeys" :value="member.key" :input-id="`member-${member.key}`" :disabled="!actionable(member)"
                        :aria-label="deviceName(member)" />
                    <button class="entity-link min-w-0" @click="openDevice(member)">
                        <span class="status-dot" :class="member.online ? 'online' : 'offline'"></span>
                        <span class="preview-name truncate">{{ deviceName(member) }}</span>
                    </button>
                </div>
                <div class="member-tags">
                    <Tag v-if="!member.online" :value="t('web.mesh_network.offline')" severity="secondary" />
                    <Tag v-else :value="member.running ? t('web.mesh_network.running') : t('web.mesh_network.stopped')" :severity="member.running ? 'success' : 'secondary'" />
                    <Tag v-if="member.pending" :value="t('web.mesh_network.pending')" severity="info" v-tooltip.top="t('web.mesh_network.pending_hint')" />
                    <Tag v-if="member.credential" :value="t('web.mesh_network.temporary')" severity="contrast" />
                    <Tag v-if="member.locked" icon="pi pi-lock" :value="t('web.mesh_network.locked')" severity="warn" v-tooltip.top="t('advanced_locked_hint')" />
                    <Tag v-if="!editable(member) && member.online" :value="t('web.mesh_network.readonly')" severity="warn" />
                </div>
                <div class="member-facts">
                    <span class="mono-value" :title="member.instanceId">{{ member.instanceId.slice(0, 8) }}</span>
                    <span v-if="member.device.public_ip" class="mono-value">{{ member.device.public_ip }}</span>
                    <span v-if="member.device.easytier_version">v{{ member.device.easytier_version.split('-')[0] }}</span>
                </div>
                <div class="member-card-actions">
                    <Button icon="pi pi-arrow-up-right" :label="t('web.mesh_network.open_device')" text size="small" severity="secondary" @click="openDevice(member)" />
                </div>
            </article>
        </div>
        <DataTable v-else v-model:selection="tableSelection" :value="members" data-key="key" class="console-table" size="small" scrollable
            :paginator="members.length > 20" :rows="20" :is-data-selectable="selectable">
            <Column selection-mode="multiple" header-style="width: 3rem" />
            <Column :header="t('web.mesh_network.devices')">
                <template #body="{ data }">
                    <button class="entity-link" @click="openDevice(data)"><span class="status-dot" :class="data.online ? 'online' : 'offline'"></span>
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
                        <Tag v-if="data.locked" icon="pi pi-lock" :value="t('web.mesh_network.locked')" severity="warn" />
                        <Tag v-if="!editable(data) && data.online" :value="t('web.mesh_network.readonly')" severity="warn" />
                    </div>
                </template>
            </Column>
            <Column field="device.easytier_version" :header="t('web.device.version')" />
            <Column :header="t('web.console.manage')">
                <template #body="{ data }"><Button icon="pi pi-arrow-up-right" text severity="secondary" v-tooltip="t('web.mesh_network.open_device')" :aria-label="t('web.mesh_network.open_device')" @click="openDevice(data)" /></template>
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
.members-hint { margin: 0; padding: 0 18px 12px; font-size: 13px; color: var(--p-text-muted-color); }
.member-cards { display: grid; grid-template-columns: repeat(auto-fill, minmax(min(100%, 300px), 1fr)); gap: 12px; padding: 0 18px 18px; }
.member-card {
    display: flex; flex-direction: column; gap: 8px; min-width: 0; padding: 14px 16px;
    border: 1px solid var(--console-border); border-radius: 8px; background: var(--console-panel);
}
.member-card.offline { opacity: .72; }
.member-card.selected { border-color: var(--p-primary-color); }
.member-card-title { display: flex; align-items: center; gap: 10px; min-width: 0; }
.member-tags { display: flex; flex-wrap: wrap; gap: 4px; }
.member-facts { display: flex; flex-wrap: wrap; gap: 10px; font-size: 12px; color: var(--p-text-muted-color); }
.member-card-actions { display: flex; justify-content: flex-end; padding-top: 8px; border-top: 1px solid var(--console-border); }
.device-pick-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(min(100%, 160px), 1fr)); gap: 8px; max-height: 18rem; overflow: auto; }
.device-pick { display: flex; align-items: center; gap: 8px; min-width: 0; padding: 8px 10px; border: 1px solid var(--console-border); border-radius: 6px; }
.device-pick label { cursor: pointer; }
.device-pick .preview-name { display: block; font-size: 13px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.device-pick .preview-secondary { display: block; font-size: 11px; }
</style>
