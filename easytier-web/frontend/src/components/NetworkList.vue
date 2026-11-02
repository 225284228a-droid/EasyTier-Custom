<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { v4 as uuidv4 } from 'uuid';
import { Button, Checkbox, Column, DataTable, Dialog, InputSwitch, InputText, Message, Skeleton, Tag, useToast } from 'primevue';
import { useRouter } from 'vue-router';
import { useI18n } from 'vue-i18n';
import { NetworkTypes, UrlListInput, Utils } from 'easytier-frontend-lib';
import ApiClient, { type CentralNetworkSettings, type GatewayInfo } from '../modules/api';
import { normalizeVirtualSubnet } from '../modules/virtual-subnet';
import { deviceConfigStore, failureMessage, useNetworkDiscovery, type DiscoveredNetwork } from '../modules/networkDiscovery';

const { t } = useI18n()
const router = useRouter();
const toast = useToast();

const props = defineProps({
    api: ApiClient,
    centralEnabled: Boolean,
});

const api = props.api;

// Networks are recognized from the configurations on the devices, the same
// way the topology recognizes running peers.
const discovery = useNetworkDiscovery();
const networks = discovery.networks;
const refreshing = discovery.refreshing;
const gatewayInfo = ref<GatewayInfo | undefined>(undefined);
const advancedMode = ref(false);
const search = ref('');
const filteredNetworks = computed(() => {
    const query = search.value.trim().toLocaleLowerCase();
    return (networks.value ?? []).filter(network =>
        `${network.name} ${network.central?.display_name ?? ''}`.toLocaleLowerCase().includes(query));
});
const deviceCount = (network: DiscoveredNetwork) => new Set(network.members.map(member => member.device.machine_id)).size;
const loadNetworks = () => discovery.refresh();

const gatewayEnabled = computed(() => gatewayInfo.value?.enabled === true);

const protos = NetworkTypes.PEER_PROTOCOLS;

const networkingMethodLabels: { [method: string]: () => string } = {
    PublicServer: () => t('public_server'),
    Manual: () => t('manual'),
    Standalone: () => t('standalone'),
    Gateway: () => t('web.network_list.gateway_mode'),
};

const networkingMethodLabel = (method: string) => {
    return networkingMethodLabels[method]?.() ?? method;
};

const createVisible = ref(false);
const creating = ref(false);
const createForm = ref({
    display_name: '',
    virtual_cidr: '',
    secure_mode: false,
});
// Initial nodes in advanced mode, same model as the GUI config form.
const initialNodes = ref<string[]>([]);

onMounted(async () => {
    void loadNetworks();
    if (!props.centralEnabled) return;
    try {
        gatewayInfo.value = await api?.get_gateway_info();
    } catch (e) {
        gatewayInfo.value = { enabled: false, relay_data: false };
    }
});

const openCreate = () => {
    createForm.value = {
        display_name: '',
        virtual_cidr: '',
        // The built-in gateway speaks the Noise handshake; third-party
        // peers of advanced networking may not, so secure mode defaults on
        // for gateway networks only.
        secure_mode: gatewayEnabled.value,
    };
    advancedMode.value = !gatewayEnabled.value;
    initialNodes.value =
        gatewayEnabled.value && gatewayInfo.value?.peer_url ? [gatewayInfo.value.peer_url] : [];
    createVisible.value = true;
};

const createNetwork = async () => {
    creating.value = true;
    try {
        const virtual_cidr = normalizeVirtualSubnet(createForm.value.virtual_cidr) || null;
        let settings: CentralNetworkSettings;
        if (!advancedMode.value && gatewayEnabled.value) {
            settings = {
                display_name: createForm.value.display_name,
                network_name: null,
                networking_method: 'Gateway',
                public_server_url: null,
                peer_urls: [],
                virtual_cidr,
                secure_mode: createForm.value.secure_mode,
            };
        } else {
            settings = {
                display_name: createForm.value.display_name,
                network_name: null,
                networking_method: initialNodes.value.length > 0 ? 'Manual' : 'Standalone',
                public_server_url: null,
                peer_urls: initialNodes.value,
                virtual_cidr,
                secure_mode: createForm.value.secure_mode,
            };
        }
        const detail = await api?.create_network(settings);
        createVisible.value = false;
        await loadNetworks();
        if (detail) {
            router.push({ name: 'networkDetail', params: { networkId: detail.network_id } });
        }
    } catch (e: any) {
        toast.add({ severity: 'error', summary: t('web.network.create'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
    } finally {
        creating.value = false;
    }
};

const toggleAdvancedMode = () => {
    advancedMode.value = !advancedMode.value;
    // Seed the gateway URL only when the list has not been edited, so
    // toggling back and forth does not drop user input.
    if (advancedMode.value && initialNodes.value.length === 0 && gatewayInfo.value?.peer_url) {
        initialNodes.value = [gatewayInfo.value.peer_url];
    }
    // Secure mode follows the networking method's default; users can flip
    // it again afterwards.
    createForm.value.secure_mode = !advancedMode.value;
};

const openNetwork = (network: DiscoveredNetwork) => {
    router.push({ name: 'meshNetwork', params: { networkName: network.name } });
};

// --- batch creation: one configuration with the same name on each device ---

const batchVisible = ref(false);
const batchCreating = ref(false);
const batchDevices = ref<Utils.DeviceInfo[]>([]);
const batchSelected = ref<string[]>([]);
const batchForm = ref({ network_name: '', network_secret: '', start: true });
const batchPeers = ref<string[]>([]);
const batchListeners = ref<string[]>([]);
const batchNameExists = computed(() =>
    (networks.value ?? []).some(network => network.name === batchForm.value.network_name.trim()));

const openBatchCreate = async () => {
    batchForm.value = { network_name: '', network_secret: uuidv4().replace(/-/g, '').slice(0, 16), start: true };
    batchPeers.value = [];
    batchListeners.value = [...NetworkTypes.DEFAULT_NETWORK_CONFIG().listener_urls];
    batchSelected.value = [];
    batchVisible.value = true;
    try {
        batchDevices.value = (await api?.list_machines() ?? [])
            .map(Utils.buildDeviceInfo)
            .filter(device => device.machine_id && device.online !== false)
            .sort((a, b) => (a.alias || a.hostname || '').localeCompare(b.alias || b.hostname || ''));
    } catch (e) {
        console.error(e);
        batchDevices.value = [];
    }
};


const batchCreate = async () => {
    const name = batchForm.value.network_name.trim();
    const devices = batchDevices.value.filter(device => batchSelected.value.includes(device.machine_id));
    if (!name || !devices.length || !api) return;
    batchCreating.value = true;
    try {
        const results = await Promise.allSettled(devices.map(async device => {
            const config = NetworkTypes.DEFAULT_NETWORK_CONFIG();
            config.network_name = name;
            config.network_secret = batchForm.value.network_secret;
            config.peer_urls = [...batchPeers.value];
            config.listener_urls = [...batchListeners.value];
            const { store } = await deviceConfigStore(api, device);
            await store.create(config, batchForm.value.start, true);
        }));
        const failed = results.filter((result): result is PromiseRejectedResult => result.status === 'rejected');
        failed.forEach(result => console.error(result.reason));
        toast.add({
            severity: failed.length ? 'warn' : 'success',
            summary: t('web.mesh_network.create'),
            detail: t('web.mesh_network.create_done', { ok: results.length - failed.length, failed: failed.length })
                + (failed.length ? `: ${failed.map(result => failureMessage(result.reason)).join('; ')}` : ''),
            life: failed.length ? 8000 : 3000,
        });
        if (failed.length < results.length) {
            batchVisible.value = false;
            await loadNetworks();
            router.push({ name: 'meshNetwork', params: { networkName: name } });
        }
    } finally {
        batchCreating.value = false;
    }
};
</script>

<template>
    <div class="console-page">
        <header class="page-heading">
            <div><h1>{{ t('web.network_list.title') }}</h1></div>
            <div class="flex flex-wrap gap-2">
                <Button v-if="centralEnabled" :label="t('web.mesh_network.create_central')" icon="pi pi-sitemap" severity="secondary" outlined @click="openCreate" />
                <Button :label="t('web.mesh_network.create')" icon="pi pi-plus" @click="openBatchCreate" />
            </div>
        </header>
        <p class="page-hint">{{ t('web.mesh_network.list_hint') }}</p>
        <Message v-if="discovery.failedDevices.value" severity="warn" :closable="false">{{ t('web.mesh_network.scan_partial', { count: discovery.failedDevices.value }) }}</Message>
        <section class="console-panel">
            <div class="list-toolbar">
                <div class="search-field"><i class="pi pi-search" aria-hidden="true"></i><InputText v-model="search" :placeholder="t('web.console.search_networks')" :aria-label="t('web.console.search_networks')" /></div>
                <div class="flex items-center gap-3"><span v-if="networks" class="table-count">{{ t('web.console.items', { count: filteredNetworks.length }) }}</span><Button icon="pi pi-refresh" text severity="secondary" :aria-label="t('web.console.refresh')" :loading="refreshing" @click="loadNetworks" /></div>
            </div>
            <div v-if="networks === undefined" class="loading-rows"><Skeleton v-for="i in 4" :key="i" height="2rem" /></div>
            <div v-else-if="networks.length === 0" class="console-empty-state"><i class="pi pi-globe" aria-hidden="true"></i><h2>{{ t('web.console.networks_empty') }}</h2><p>{{ t('web.console.networks_empty_hint') }}</p><Button :label="t('web.mesh_network.create')" icon="pi pi-plus" @click="openBatchCreate" /></div>
            <div v-else-if="filteredNetworks.length === 0" class="console-empty-state"><h2>{{ t('web.console.no_results') }}</h2><Button :label="t('web.console.clear_search')" text @click="search = ''" /></div>
            <template v-else>
                <DataTable :value="filteredNetworks" dataKey="identity" class="console-table desktop-list" size="small" scrollable :paginator="filteredNetworks.length > 20" :rows="20">
                    <Column field="name" :header="t('web.mesh_network.network_name')" sortable><template #body="{ data }"><button class="entity-link" @click="openNetwork(data)"><i class="pi pi-globe entity-icon" aria-hidden="true"></i>{{ data.name }}</button></template></Column>
                    <Column :header="t('web.mesh_network.devices')"><template #body="{ data }">{{ deviceCount(data) }}</template></Column>
                    <Column field="runningCount" :header="t('web.mesh_network.running')" sortable><template #body="{ data }">{{ data.runningCount }} <span class="muted">/ {{ data.members.length }}</span></template></Column>
                    <Column :header="t('networking_method')"><template #body="{ data }"><Tag v-if="data.central" :value="`${t('web.mesh_network.central')} · ${networkingMethodLabel(data.central.networking_method)}`" severity="info" /></template></Column>
                    <Column :header="t('web.console.manage')"><template #body="{ data }"><Button icon="pi pi-arrow-up-right" text severity="secondary" :aria-label="`${t('web.console.manage')} ${data.name}`" @click="openNetwork(data)" /></template></Column>
                </DataTable>
                <div class="mobile-list">
                    <article v-for="network in filteredNetworks" :key="network.identity" class="mobile-list-item">
                        <div class="flex items-center justify-between gap-3"><button class="entity-link" @click="openNetwork(network)"><i class="pi pi-globe entity-icon" aria-hidden="true"></i>{{ network.name }}</button><Tag v-if="network.central" :value="t('web.mesh_network.central')" severity="info" /></div>
                        <p class="preview-secondary">{{ t('web.mesh_network.devices') }} · {{ deviceCount(network) }} · {{ t('web.mesh_network.running') }} {{ network.runningCount }} / {{ network.members.length }}</p>
                    </article>
                </div>
            </template>
        </section>

        <Dialog v-model:visible="createVisible" modal class="console-dialog" :header="t('web.network_list.create')" :style="{ width: '32rem' }">
            <div class="flex flex-col gap-3">
                <div v-if="gatewayEnabled && !advancedMode" class="flex items-start gap-2 p-3 surface-100 rounded-md">
                    <i class="pi pi-check-circle text-green-500 mt-0.5"></i>
                    <div class="text-sm">
                        {{ t('web.network_list.gateway_hint') }}
                        <span v-if="gatewayInfo?.peer_url" class="block font-mono text-xs mt-1">{{ gatewayInfo.peer_url }}</span>
                    </div>
                </div>
                <div class="flex flex-col gap-1">
                    <label for="network-display-name">{{ t('web.network_list.display_name') }}</label>
                    <InputText id="network-display-name" v-model="createForm.display_name" />
                </div>
                <div class="flex flex-col gap-1">
                    <div class="flex items-center">
                        <label for="network-cidr">{{ t('web.network_list.virtual_cidr') }}</label>
                        <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('web.network_list.virtual_cidr_hint')"></span>
                    </div>
                    <InputText id="network-cidr" v-model="createForm.virtual_cidr" class="font-mono"
                        @blur="createForm.virtual_cidr = normalizeVirtualSubnet(createForm.virtual_cidr)"
                        :placeholder="t('web.network_list.virtual_cidr_placeholder')" />
                </div>
                <div class="flex items-center gap-2">
                    <InputSwitch inputId="create-secure-mode" v-model="createForm.secure_mode" />
                    <label for="create-secure-mode" class="text-sm">{{ t('web.network_list.secure_mode') }}</label>
                    <span class="pi pi-question-circle text-sm" v-tooltip.top="t('web.network_list.secure_mode_hint')"></span>
                </div>
                <template v-if="advancedMode || !gatewayEnabled">
                    <div class="flex items-center">
                        <label for="initial-nodes">{{ t('initial_nodes') }}</label>
                        <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('initial_nodes_help')"></span>
                    </div>
                    <div class="items-center flex flex-col p-fluid gap-y-2">
                        <UrlListInput id="initial-nodes" v-model="initialNodes" :protos="protos"
                            defaultUrl="tcp://:11010" :add-label="t('add_initial_node')"
                            :placeholder="t('initial_node_placeholder')" />
                    </div>
                </template>
                <div v-if="gatewayEnabled" class="text-sm">
                    <button type="button" class="text-primary text-left" @click="toggleAdvancedMode">
                        {{ advancedMode ? t('web.network_list.use_gateway') : t('web.network_list.advanced_mode') }}
                    </button>
                </div>
                <div class="text-sm text-500">{{ t('web.network_list.create_hint') }}</div>
            </div>
            <template #footer>
                <Button :label="t('web.common.cancel')" icon="pi pi-times" text severity="secondary" @click="createVisible = false" />
                <Button :label="t('web.common.confirm')" icon="pi pi-check" :loading="creating" @click="createNetwork" />
            </template>
        </Dialog>

        <Dialog v-model:visible="batchVisible" modal class="console-dialog" :header="t('web.mesh_network.create')" :style="{ width: '36rem' }">
            <div class="flex flex-col gap-3">
                <div class="flex flex-col gap-1">
                    <label for="batch-network-name">{{ t('web.mesh_network.network_name') }}</label>
                    <InputText id="batch-network-name" v-model="batchForm.network_name" class="font-mono" />
                    <small v-if="batchNameExists" class="text-sm text-500">{{ t('web.mesh_network.name_exists') }}</small>
                </div>
                <div class="flex flex-col gap-1">
                    <label for="batch-network-secret">{{ t('web.mesh_network.network_secret') }}</label>
                    <InputText id="batch-network-secret" v-model="batchForm.network_secret" class="font-mono" />
                </div>
                <div class="flex items-center">
                    <label for="batch-initial-nodes">{{ t('initial_nodes') }}</label>
                    <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('initial_nodes_help')"></span>
                </div>
                <UrlListInput id="batch-initial-nodes" v-model="batchPeers" :protos="protos"
                    defaultUrl="tcp://:11010" :add-label="t('add_initial_node')" :placeholder="t('initial_node_placeholder')" />
                <label for="batch-listeners">{{ t('web.mesh_network.listeners') }}</label>
                <UrlListInput id="batch-listeners" v-model="batchListeners" :protos="protos"
                    defaultUrl="tcp://0.0.0.0:11010" :add-label="t('web.common.add')" placeholder="tcp://0.0.0.0:11010" />
                <div class="flex flex-col gap-1">
                    <span>{{ t('web.mesh_network.target_devices') }}</span>
                    <div v-if="batchDevices.length === 0" class="text-sm text-500">{{ t('web.mesh_network.no_online_devices') }}</div>
                    <div v-else class="batch-device-grid">
                        <div v-for="device in batchDevices" :key="device.machine_id" class="batch-device">
                            <Checkbox v-model="batchSelected" :value="device.machine_id" :inputId="`batch-device-${device.machine_id}`" />
                            <label :for="`batch-device-${device.machine_id}`" class="min-w-0"><span class="preview-name">{{ device.alias || device.hostname || device.machine_id }}</span>
                                <span class="preview-secondary">{{ t('web.console.instances') }} · {{ device.running_network_count }}</span></label>
                        </div>
                    </div>
                </div>
                <div class="flex items-center gap-2">
                    <InputSwitch inputId="batch-start" v-model="batchForm.start" />
                    <label for="batch-start" class="text-sm">{{ t('web.mesh_network.start_after_create') }}</label>
                </div>
                <div class="text-sm text-500">{{ t('web.mesh_network.create_hint') }}</div>
            </div>
            <template #footer>
                <Button :label="t('web.common.cancel')" icon="pi pi-times" text severity="secondary" @click="batchVisible = false" />
                <Button :label="t('web.common.confirm')" icon="pi pi-check" :loading="batchCreating"
                    :disabled="!batchForm.network_name.trim() || batchSelected.length === 0" @click="batchCreate" />
            </template>
        </Dialog>
    </div>
</template>

<style scoped>
.page-hint { margin: -8px 0 0; font-size: 13px; color: var(--p-text-muted-color); }
.batch-device-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(min(100%, 150px), 1fr)); gap: 8px; max-height: 14rem; overflow: auto; }
.batch-device { display: flex; align-items: center; gap: 8px; min-width: 0; padding: 8px 10px; border: 1px solid var(--console-border); border-radius: 6px; }
.batch-device label { cursor: pointer; }
.batch-device .preview-name { display: block; font-size: 13px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.batch-device .preview-secondary { display: block; font-size: 11px; }
</style>
