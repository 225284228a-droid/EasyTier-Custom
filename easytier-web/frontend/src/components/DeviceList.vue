<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue';
import { Button, Checkbox, Column, DataTable, Dialog, InputText, SelectButton, Skeleton, useToast } from 'primevue';
import Tooltip from 'primevue/tooltip';
import { Utils } from 'easytier-frontend-lib';
import DeviceDetails from './DeviceDetails.vue';
import EntityName from './EntityName.vue';
import { useI18n } from 'vue-i18n'
import ApiClient from '../modules/api';
import { useDevicePopup } from '../modules/devicePopup';

const { t } = useI18n()

// 注册 Tooltip 指令
const vTooltip = Tooltip;

const props = defineProps({
    api: ApiClient,
    centralEnabled: Boolean,
});

const api = props.api;
const toast = useToast();
const devicePopup = useDevicePopup();

const deviceList = ref<Array<Utils.DeviceInfo> | undefined>(undefined);
const search = ref('');
const loadError = ref(false);
const refreshing = ref(false);
let requestInFlight = false;
const expandedRows = ref({});

// 桌面端默认卡片视图，可切换到表格；选择持久化；移动端始终使用紧凑列表
const viewMode = ref<'table' | 'card'>(localStorage.getItem('deviceList.viewMode') === 'table' ? 'table' : 'card');
const viewModeOptions = [
    { value: 'card', icon: 'pi pi-th-large' },
    { value: 'table', icon: 'pi pi-table' },
];
const viewModeLabel = (value: string) => t(value === 'card' ? 'web.console.view_card' : 'web.console.view_table');
watch(viewMode, (mode) => localStorage.setItem('deviceList.viewMode', mode));

// Zero-device state is where users look for "how do I add one": surface
// the enrollment command right there.
const enrollInfo = ref<Awaited<ReturnType<ApiClient['get_console_info']>> | null>(null);
const enrollCommand = computed(() => {
    if (!enrollInfo.value) return '';
    const host = window.location.hostname;
    const { config_server_protocol: proto, config_server_port: port, username } = enrollInfo.value;
    return `easytier-core --config-server ${proto}://${host}:${port}/${username}`;
});

const copyEnrollCommand = async () => {
    try {
        await navigator.clipboard.writeText(enrollCommand.value);
        toast.add({ severity: 'success', summary: t('web.common.confirm'), life: 1500 });
    } catch (e) {
        console.error(e);
    }
};

const loadDevices = async (manual = false) => {
    // A manual refresh can show progress for an already running background request.
    if (manual) refreshing.value = true;
    if (requestInFlight) return;
    requestInFlight = true;
    try {
        const resp = await api?.list_machines();
        deviceList.value = (resp || []).map(Utils.buildDeviceInfo);
        loadError.value = false;
    } catch (e) {
        loadError.value = true;
        console.error(e);
    } finally {
        requestInFlight = false;
        refreshing.value = false;
    }
};

const periodFunc = new Utils.PeriodicTask(async () => {
    try {
        await loadDevices();
    } catch (e) {
        console.error(e);
    }
}, 1000);

onMounted(() => {
    periodFunc.start();
    api?.get_console_info().then(info => { enrollInfo.value = info; }).catch(() => {});
});

onUnmounted(() => {
    periodFunc.stop();
});

// 设备管理在当前页面上方打开，关闭后停留在原位置；“编辑”直接打开配置表单
const openStatus = (device: Utils.DeviceInfo) => {
    devicePopup.open(device.machine_id, device.running_network_instances?.[0]);
};
const openEditor = (device: Utils.DeviceInfo) => {
    devicePopup.open(device.machine_id, device.running_network_instances?.[0], true);
};

// 删除设备：若还是网络成员会连带移出并回收托管配置；可选同时拉黑
const deleteVisible = ref(false);
const deleting = ref(false);
const deleteTarget = ref<Utils.DeviceInfo | null>(null);
const deleteAlsoBlock = ref(true);

const handleDeleteDevice = (device: Utils.DeviceInfo) => {
    deleteTarget.value = device;
    deleteAlsoBlock.value = true;
    deleteVisible.value = true;
};

// 别名替代主机名显示，并作为设备在网络内的名称（手动改过的除外）
const deviceName = (device: Utils.DeviceInfo) => device.alias || device.hostname || device.machine_id;
const lastSeen = (device: Utils.DeviceInfo) => device.last_seen?.replace('T', ' ').slice(0, 16) || '—';
const version = (device: Utils.DeviceInfo) => device.easytier_version ? `v${device.easytier_version.split('-')[0]}` : '—';

const renameDevice = (device: Utils.DeviceInfo) => async (value: string) => {
    if (!api || value === (device.alias ?? '')) return;
    try {
        await api.update_machine_alias(device.machine_id, value);
        await loadDevices();
    } catch (e: any) {
        toast.add({ severity: 'error', summary: t('web.console.rename'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
        throw e;
    }
};

const confirmDeleteDevice = async () => {
    if (!deleteTarget.value) return;
    deleting.value = true;
    try {
        await api?.delete_machine(deleteTarget.value.machine_id, deleteAlsoBlock.value);
        deleteVisible.value = false;
        await loadDevices();
    } catch (e: any) {
        toast.add({ severity: 'error', summary: t('web.console.delete_device'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
    } finally {
        deleting.value = false;
    }
};

// 搜索过滤 + 预排序：离线设备始终排在在线设备之后；列排序交给 DataTable
const sortedDeviceList = computed(() => {
    const query = search.value.trim().toLocaleLowerCase();
    return (deviceList.value ?? [])
        .filter(device => `${device.alias ?? ''} ${device.hostname ?? ''} ${device.public_ip ?? ''}`.toLocaleLowerCase().includes(query))
        .sort((a, b) => {
            const online = Number(b.online === true) - Number(a.online === true);
            if (online !== 0) return online;
            return (deviceName(a) ?? '').localeCompare(deviceName(b) ?? '');
        });
});

const locationText = (device: Utils.DeviceInfo) => {
    const parts = [device.location?.country, device.location?.region, device.location?.city].filter(Boolean);
    return parts.length ? parts.join(' · ') : t('web.device.unknown_location');
};
</script>

<template>
    <div class="console-page">
        <header class="page-heading">
            <div>
                <h1>{{ t('web.device.list') }}</h1>
            </div>
        </header>
        <section class="console-panel">
            <div class="list-toolbar">
                <span class="search-field"><i class="pi pi-search" aria-hidden="true" /><InputText v-model="search"
                        :placeholder="t('web.console.search_devices')"
                        :aria-label="t('web.console.search_devices')" /></span>
                <div class="flex items-center gap-3">
                    <span v-if="deviceList" class="table-count">{{ t('web.console.items', { count: sortedDeviceList.length }) }}</span>
                    <SelectButton v-model="viewMode" :options="viewModeOptions" optionValue="value"
                        :allow-empty="false" class="view-toggle">
                        <template #option="{ option }">
                            <i :class="option.icon" v-tooltip.top="viewModeLabel(option.value)"
                                :aria-label="viewModeLabel(option.value)"></i>
                        </template>
                    </SelectButton>
                    <Button icon="pi pi-refresh" text severity="secondary" :aria-label="t('web.console.refresh')"
                        :loading="refreshing" @click="loadDevices(true)" />
                </div>
            </div>
            <div v-if="deviceList === undefined && !loadError" class="loading-rows">
                <Skeleton v-for="i in 4" :key="i" height="2rem" />
            </div>
            <div v-else-if="loadError" class="console-empty-state">
                <i class="pi pi-exclamation-triangle" aria-hidden="true"></i>
                <h2>{{ t('web.console.load_failed') }}</h2>
                <Button :label="t('web.console.retry')" icon="pi pi-refresh" @click="loadDevices(true)" />
            </div>
            <div v-else-if="sortedDeviceList.length === 0" class="console-empty-state">
                <i class="pi pi-server" aria-hidden="true"></i>
                <h2>{{ search ? t('web.console.no_results') : t('web.console.devices_empty') }}</h2>
                <p v-if="!search">{{ t('web.console.devices_empty_hint') }}</p>
                <div v-if="!search && enrollCommand" class="flex items-center gap-2 justify-center mt-2 flex-wrap">
                    <code class="mono-value p-2 rounded" style="background: var(--console-ground); word-break: break-all">{{ enrollCommand }}</code>
                    <Button icon="pi pi-copy" severity="secondary" outlined size="small"
                        :aria-label="t('web.console.enroll_copy')" @click="copyEnrollCommand" />
                </div>
            </div>
            <template v-else>
                <DataTable v-if="viewMode === 'table'" :value="sortedDeviceList" class="console-table desktop-list" size="small"
                    v-model:expandedRows="expandedRows" dataKey="machine_id" scrollable
                    :paginator="sortedDeviceList.length > 20" :rows="20">
                    <Column expander style="width: 2.5rem" />
                    <Column field="hostname" :header="t('web.device.hostname')" sortable>
                        <template #body="{ data }">
                            <div class="device-name-cell">
                                <span class="status-dot" :class="data.online ? 'online' : 'offline'"></span>
                                <EntityName :name="deviceName(data)" :secondary="data.alias ? data.hostname : undefined" :value="data.alias ?? ''"
                                    :placeholder="data.hostname" :maxlength="64" :rename="centralEnabled ? renameDevice(data) : undefined" />
                            </div>
                        </template>
                    </Column>
                    <Column :header="t('web.console.location')">
                        <template #body="{ data }">{{ locationText(data) }}</template>
                    </Column>
                    <Column field="running_network_count" :header="t('web.device.networks')" sortable style="width: 7rem" />
                    <Column field="easytier_version" :header="t('web.device.version')" sortable style="width: 7rem">
                        <template #body="{ data }"><span class="mono-value muted">v{{ (data.easytier_version || '?').split('-')[0] }}</span></template>
                    </Column>
                    <Column :header="t('web.device.last_seen')" style="width: 10rem">
                        <template #body="{ data }"><span class="muted">{{ lastSeen(data) }}</span></template>
                    </Column>
                    <Column :header="t('web.console.manage')" style="width: 8rem">
                        <template #body="{ data }">
                            <div class="flex items-center gap-1">
                                <Button v-tooltip.top="t('web.mesh_network.status')" icon="pi pi-chart-line" severity="secondary" text rounded
                                    :aria-label="t('web.mesh_network.status')" @click="openStatus(data)" />
                                <Button v-tooltip.top="t('web.console.edit')" icon="pi pi-pencil" severity="secondary" text rounded
                                    :aria-label="t('web.console.edit')" @click="openEditor(data)" />
                                <Button v-if="centralEnabled" v-tooltip.top="t('web.console.delete_device')" icon="pi pi-trash" severity="danger"
                                    text rounded :aria-label="t('web.console.delete_device')" @click="handleDeleteDevice(data)" />
                            </div>
                        </template>
                    </Column>
                    <template #expansion="{ data }">
                        <DeviceDetails :device="data" containerClass="row-details-content" :compact="true" />
                    </template>
                </DataTable>
                <div v-if="viewMode === 'card'" class="entity-cards device-cards">
                    <article v-for="device in sortedDeviceList" :key="device.machine_id" class="entity-card">
                        <header class="entity-card-head">
                            <span class="status-dot" :class="device.online ? 'online' : 'offline'" role="img"
                                :aria-label="t(device.online ? 'web.mesh_network.running' : 'web.mesh_network.offline')"></span>
                            <EntityName :name="deviceName(device)" :secondary="device.alias ? device.hostname : undefined"
                                :value="device.alias ?? ''" :placeholder="device.hostname" :maxlength="64"
                                :rename="centralEnabled ? renameDevice(device) : undefined" />
                        </header>
                        <dl class="entity-facts">
                            <div><dt>{{ t('web.console.location') }}</dt><dd :title="locationText(device)">{{ locationText(device) }}</dd></div>
                            <div><dt>{{ t('web.device.version') }}</dt><dd class="mono-value">{{ version(device) }}</dd></div>
                            <div><dt>{{ t('web.device.networks') }}</dt><dd>{{ device.running_network_count }}</dd></div>
                            <div><dt>{{ t('web.device.last_seen') }}</dt><dd>{{ lastSeen(device) }}</dd></div>
                        </dl>
                        <footer class="entity-card-foot">
                            <div class="entity-actions">
                                <Button icon="pi pi-chart-line" :label="t('web.mesh_network.status')" text size="small" severity="secondary" @click="openStatus(device)" />
                                <Button icon="pi pi-pencil" :label="t('web.console.edit')" text size="small" severity="secondary" @click="openEditor(device)" />
                                <Button v-if="centralEnabled" icon="pi pi-trash" :label="t('web.console.delete')" text size="small" severity="danger"
                                    :aria-label="t('web.console.delete_device')" @click="handleDeleteDevice(device)" />
                            </div>
                        </footer>
                    </article>
                </div>
                <div class="mobile-list">
                    <article v-for="device in sortedDeviceList" :key="device.machine_id" class="mobile-list-item">
                        <div class="device-name-cell">
                            <span class="status-dot" :class="device.online ? 'online' : 'offline'"></span>
                            <EntityName :name="deviceName(device)" :secondary="device.alias ? device.hostname : undefined" :value="device.alias ?? ''"
                                :placeholder="device.hostname" :maxlength="64" :rename="centralEnabled ? renameDevice(device) : undefined" />
                            <span class="mono-value muted shrink-0">{{ version(device) }}</span>
                        </div>
                        <p class="preview-secondary">{{ locationText(device) }} · {{ device.running_network_count }} {{ t('web.console.instances') }}</p>
                        <div class="mobile-actions">
                            <Button icon="pi pi-chart-line" :label="t('web.mesh_network.status')" text size="small" severity="secondary" @click="openStatus(device)" />
                            <Button icon="pi pi-pencil" :label="t('web.console.edit')" text size="small" severity="secondary" @click="openEditor(device)" />
                            <Button v-if="centralEnabled" icon="pi pi-trash" :label="t('web.console.delete')" text size="small" severity="danger"
                                :aria-label="t('web.console.delete_device')" @click="handleDeleteDevice(device)" />
                        </div>
                        <details>
                            <summary>{{ t('web.console.details') }}</summary>
                            <DeviceDetails :device="device" :compact="true" />
                        </details>
                    </article>
                </div>
            </template>
        </section>

        <!-- 删除确认对话框 -->
        <Dialog v-model:visible="deleteVisible" modal :header="t('web.console.delete_device')" :style="{ width: '26rem' }">
            <div class="flex flex-col gap-3">
                <div>{{ t('web.console.delete_device_confirm', { device: deleteTarget?.hostname || deleteTarget?.machine_id }) }}</div>
                <div class="flex items-center gap-2">
                    <Checkbox inputId="delete-also-block" v-model="deleteAlsoBlock" :binary="true" />
                    <label for="delete-also-block">{{ t('web.console.block_device') }}</label>
                </div>
                <div class="text-xs text-500">{{ deleteAlsoBlock ? t('web.console.block_device_hint') : t('web.console.no_block_device_hint') }}</div>
            </div>
            <template #footer>
                <Button :label="t('web.common.cancel')" icon="pi pi-times" text severity="secondary" @click="deleteVisible = false" />
                <Button :label="t('web.common.confirm')" icon="pi pi-check" :loading="deleting" severity="danger" @click="confirmDeleteDevice" />
            </template>
        </Dialog>

    </div>
</template>

<style scoped>
.device-name-cell { display: flex; align-items: flex-start; gap: 10px; min-width: 0; }
.device-name-cell > .status-dot { margin-top: 6px; }
.mobile-actions { display: flex; flex-wrap: wrap; gap: 2px; margin: 6px -8px 0; }
@media (max-width: 767px) {
    .device-cards { display: none; }
}
</style>
