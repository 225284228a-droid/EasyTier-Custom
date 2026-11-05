<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';
import { v4 as uuidv4 } from 'uuid';
import { Button, Checkbox, Column, DataTable, Dialog, InputSwitch, InputText, Message, Password, SelectButton, Skeleton, useToast } from 'primevue';
import { useRouter } from 'vue-router';
import { useI18n } from 'vue-i18n';
import { NetworkTypes, UrlListInput, Utils } from 'easytier-frontend-lib';
import ApiClient, { type GatewayInfo } from '../modules/api';
import { parseVirtualSubnet } from '../modules/cidr';
import { deviceConfigStore, failureMessage, useNetworkDiscovery, type DiscoveredNetwork } from '../modules/networkDiscovery';
import { failedResults } from '../modules/meshNetworkContext';

const { t } = useI18n()
const router = useRouter();
const toast = useToast();

const props = defineProps<{ api: ApiClient; centralEnabled?: boolean }>();

// Networks are recognized from the configurations on the devices, the same
// way the topology recognizes running peers.
const discovery = useNetworkDiscovery();
const networks = discovery.networks;
const search = ref('');
const filteredNetworks = computed(() => {
    const query = search.value.trim().toLocaleLowerCase();
    return (networks.value ?? []).filter(network =>
        `${network.name} ${network.label}`.toLocaleLowerCase().includes(query));
});
const deviceCount = (network: DiscoveredNetwork) => new Set(network.members.map(member => member.device.machine_id)).size;
const temporaryCount = (network: DiscoveredNetwork) => network.members.filter(member => member.credential).length;
const openNetwork = (network: DiscoveredNetwork) => router.push({ name: 'meshNetwork', params: { networkName: network.name } });

const viewMode = ref<'table' | 'card'>(localStorage.getItem('networkList.viewMode') === 'table' ? 'table' : 'card');
watch(viewMode, mode => localStorage.setItem('networkList.viewMode', mode));
const viewModeOptions = [
    { value: 'card', icon: 'pi pi-th-large' },
    { value: 'table', icon: 'pi pi-table' },
];
const viewModeLabel = (value: string) => t(value === 'card' ? 'web.console.view_card' : 'web.console.view_table');

const gatewayInfo = ref<GatewayInfo>();
const loadGatewayInfo = async () => {
    if (!props.centralEnabled || gatewayInfo.value) return;
    gatewayInfo.value = await props.api.get_gateway_info().catch(() => ({ enabled: false, relay_data: false }));
};
// The console mode can become known after the page opened.
watch(() => props.centralEnabled, () => void loadGatewayInfo());
onMounted(() => {
    void discovery.refresh();
    void loadGatewayInfo();
});

// --- a new network: only the name is required ---

const protos = NetworkTypes.PEER_PROTOCOLS;
const createVisible = ref(false);
const creating = ref(false);
const form = ref({ network_name: '', display_name: '', network_secret: '', no_password: false, secure_mode: false, virtual_subnet: '' });
// Empty leaves DHCP at its default; devices start in the subnet otherwise.
const subnet = computed(() => parseVirtualSubnet(form.value.virtual_subnet));
const peers = ref<string[]>([]);
const listeners = ref<string[]>([]);
const devices = ref<Utils.DeviceInfo[]>();
const selectedDevices = ref<string[]>([]);
const nameExists = computed(() => (networks.value ?? []).some(network => network.name === form.value.network_name.trim()));
// Without central management the network exists only on devices.
const needsDevices = computed(() => !props.centralEnabled);

const openCreate = async () => {
    form.value = { network_name: '', display_name: '', network_secret: '', no_password: false, secure_mode: false, virtual_subnet: '' };
    await loadGatewayInfo();
    peers.value = gatewayInfo.value?.enabled && gatewayInfo.value.peer_url ? [gatewayInfo.value.peer_url] : [];
    // A new network listens on TCP and UDP; WireGuard is opt-in per device.
    listeners.value = NetworkTypes.DEFAULT_NETWORK_CONFIG().listener_urls.filter(url => !url.startsWith('wg://'));
    selectedDevices.value = [];
    devices.value = undefined;
    createVisible.value = true;
    try {
        devices.value = (await props.api.list_machines()).map(Utils.buildDeviceInfo)
            .filter(device => device.machine_id && device.online !== false)
            .sort((a, b) => (a.alias || a.hostname || '').localeCompare(b.alias || b.hostname || ''));
    } catch (error) {
        console.error(error);
        devices.value = [];
    }
};

const createOnDevices = async (name: string) => {
    // A network needs one secret on every device; it is chosen here.
    const secret = form.value.no_password ? '' : form.value.network_secret.trim() || uuidv4().replace(/-/g, '').slice(0, 16);
    const targets = (devices.value ?? []).filter(device => selectedDevices.value.includes(device.machine_id));
    const results = await Promise.allSettled(targets.map(async device => {
        const config = NetworkTypes.DEFAULT_NETWORK_CONFIG();
        config.network_name = name;
        const displayName = form.value.display_name.trim();
        if (displayName) config.instance_name = displayName;
        config.network_secret = secret;
        config.peer_urls = [...peers.value];
        config.listener_urls = [...listeners.value];
        if (subnet.value) config.dhcp_ipv4_cidr = subnet.value;
        const { store } = await deviceConfigStore(props.api, device);
        await store.create(config, true, true);
    }));
    return results.flatMap(result => result.status === 'rejected' ? [failureMessage(result.reason)] : []);
};

const create = async () => {
    const name = form.value.network_name.trim();
    if (!name || nameExists.value || subnet.value === null) return;
    creating.value = true;
    try {
        let failures: string[];
        if (props.centralEnabled) {
            const results = await props.api.create_mesh_network({
                network_name: name,
                display_name: form.value.display_name.trim() || undefined,
                network_secret: form.value.no_password ? '' : form.value.network_secret.trim() || undefined,
                secure_mode: form.value.secure_mode,
                preset: { peers: [...peers.value], listeners: [...listeners.value], advanced: {} },
                virtual_subnet: subnet.value,
                devices: selectedDevices.value,
            });
            failures = failedResults(results).map(result => result.message ?? result.status);
        } else {
            failures = await createOnDevices(name);
        }
        toast.add({
            severity: failures.length ? 'warn' : 'success',
            summary: t('web.mesh_network.create'),
            detail: failures.length ? failures.join('; ') : t('web.mesh_network.created', { name }),
            life: failures.length ? 8000 : 3000,
        });
        createVisible.value = false;
        await discovery.refresh();
        await router.push({ name: 'meshNetwork', params: { networkName: name } });
    } catch (error) {
        toast.add({ severity: 'error', summary: t('web.mesh_network.create'), detail: failureMessage(error), life: 5000 });
    } finally {
        creating.value = false;
    }
};
</script>

<template>
    <div class="console-page">
        <header class="page-heading">
            <div><h1>{{ t('web.network_list.title') }}</h1></div>
            <div class="flex flex-wrap gap-2">
                <Button :label="t('web.mesh_network.create')" icon="pi pi-plus" @click="openCreate" />
            </div>
        </header>
        <p class="page-hint">{{ t('web.mesh_network.list_hint') }}</p>
        <Message v-if="discovery.failedDevices.value" severity="warn" :closable="false">{{ t('web.mesh_network.scan_partial', { count: discovery.failedDevices.value }) }}</Message>
        <section class="console-panel">
            <div class="list-toolbar">
                <div class="search-field"><i class="pi pi-search" aria-hidden="true"></i><InputText v-model="search" :placeholder="t('web.console.search_networks')" :aria-label="t('web.console.search_networks')" /></div>
                <div class="flex items-center gap-3">
                    <span v-if="networks" class="table-count">{{ t('web.console.items', { count: filteredNetworks.length }) }}</span>
                    <SelectButton v-model="viewMode" :options="viewModeOptions" option-value="value" :allow-empty="false" size="small" class="view-toggle">
                        <template #option="{ option }">
                            <i :class="option.icon" v-tooltip.top="viewModeLabel(option.value)" :aria-label="viewModeLabel(option.value)"></i>
                        </template>
                    </SelectButton>
                    <Button icon="pi pi-refresh" text severity="secondary" :aria-label="t('web.console.refresh')" :loading="discovery.refreshing.value" @click="discovery.refresh()" />
                </div>
            </div>
            <div v-if="networks === undefined" class="loading-rows"><Skeleton v-for="i in 4" :key="i" height="2rem" /></div>
            <div v-else-if="networks.length === 0" class="console-empty-state"><i class="pi pi-globe" aria-hidden="true"></i><h2>{{ t('web.console.networks_empty') }}</h2><p>{{ t('web.console.networks_empty_hint') }}</p><Button :label="t('web.mesh_network.create')" icon="pi pi-plus" @click="openCreate" /></div>
            <div v-else-if="filteredNetworks.length === 0" class="console-empty-state"><h2>{{ t('web.console.no_results') }}</h2><Button :label="t('web.console.clear_search')" text @click="search = ''" /></div>
            <div v-else-if="viewMode === 'card'" class="entity-cards network-cards">
                <article v-for="network in filteredNetworks" :key="network.identity" class="entity-card network-card">
                    <header class="entity-card-head">
                        <span class="status-dot" :class="network.runningCount ? 'online' : 'offline'" aria-hidden="true"></span>
                        <div class="entity-names">
                            <button type="button" class="entity-name network-open" @click="openNetwork(network)"><span>{{ network.label }}</span></button>
                            <span v-if="network.label !== network.name" class="entity-sub mono-value">{{ network.name }}</span>
                        </div>
                    </header>
                    <dl class="entity-facts network-facts">
                        <div><dt>{{ t('web.mesh_network.devices') }}</dt><dd>{{ deviceCount(network) }}</dd></div>
                        <div><dt>{{ t('web.mesh_network.running') }}</dt><dd>{{ network.runningCount }}<span class="muted"> / {{ network.members.length }}</span></dd></div>
                        <div><dt>{{ t('web.mesh_network.temporary') }}</dt><dd>{{ temporaryCount(network) }}</dd></div>
                    </dl>
                    <footer class="entity-card-foot">
                        <div class="entity-actions">
                            <Button icon="pi pi-arrow-right" :label="t('web.console.manage')" text size="small" severity="secondary" @click="openNetwork(network)" />
                        </div>
                    </footer>
                </article>
            </div>
            <DataTable v-else :value="filteredNetworks" data-key="identity" class="console-table" size="small" scrollable :paginator="filteredNetworks.length > 20" :rows="20">
                <Column field="name" :header="t('web.mesh_network.network_name')" sortable><template #body="{ data }"><button class="entity-link" @click="openNetwork(data)"><i class="pi pi-globe entity-icon" aria-hidden="true"></i>{{ data.label }}</button><span v-if="data.label !== data.name" class="preview-secondary mono-value block">{{ data.name }}</span></template></Column>
                <Column :header="t('web.mesh_network.devices')"><template #body="{ data }">{{ deviceCount(data) }}</template></Column>
                <Column field="runningCount" :header="t('web.mesh_network.running')" sortable><template #body="{ data }">{{ data.runningCount }} <span class="muted">/ {{ data.members.length }}</span></template></Column>
                <Column :header="t('web.mesh_network.temporary')"><template #body="{ data }">{{ temporaryCount(data) }}</template></Column>
                <Column :header="t('web.console.manage')"><template #body="{ data }"><Button icon="pi pi-arrow-up-right" text severity="secondary" :aria-label="`${t('web.console.manage')} ${data.name}`" @click="openNetwork(data)" /></template></Column>
            </DataTable>
        </section>

        <Dialog v-model:visible="createVisible" modal class="console-dialog" :header="t('web.mesh_network.create')" :style="{ width: '40rem', maxWidth: 'calc(100vw - 32px)' }">
            <div class="flex flex-col gap-3">
                <div class="flex flex-col gap-1">
                    <label for="create-network-name">{{ t('web.mesh_network.network_name') }} <span class="required">*</span></label>
                    <InputText id="create-network-name" v-model="form.network_name" class="font-mono" :invalid="nameExists" autofocus />
                    <small v-if="nameExists" class="text-red-500">{{ t('web.mesh_network.name_exists') }}</small>
                </div>
                <div class="flex flex-col gap-1">
                    <label for="create-display-name">{{ t('web.mesh_network.display_name') }}</label>
                    <InputText id="create-display-name" v-model="form.display_name" :placeholder="form.network_name" />
                </div>
                <div class="flex flex-col gap-1">
                    <label for="create-network-secret">{{ t('web.mesh_network.network_secret') }}</label>
                    <div class="flex flex-col sm:flex-row gap-2">
                        <Password input-id="create-network-secret" v-model="form.network_secret" :placeholder="t('web.mesh_network.secret_generated')"
                            toggle-mask :feedback="false" fluid class="flex-1 min-w-0" :disabled="creating || form.no_password"
                            :input-props="{ autocomplete: 'new-password' }" />
                        <Button :label="t('web.network_list.generate_secret')" icon="pi pi-refresh" severity="secondary" outlined
                            :disabled="creating || form.no_password" @click="form.network_secret = uuidv4()" />
                    </div>
                    <div class="flex items-center gap-2">
                        <Checkbox input-id="create-no-password" v-model="form.no_password" binary :disabled="creating" />
                        <label for="create-no-password" class="text-sm">{{ t('web.network_list.no_password') }}</label>
                    </div>
                </div>
                <div class="flex items-center gap-2">
                    <InputSwitch input-id="create-secure-mode" v-model="form.secure_mode" />
                    <label for="create-secure-mode" class="text-sm">{{ t('web.mesh_network.secure_mode') }}</label>
                    <span class="pi pi-question-circle text-sm" v-tooltip.top="t('web.network_list.secure_mode_hint')"></span>
                </div>
                <div class="flex flex-col gap-1">
                    <label for="create-virtual-subnet">{{ t('web.mesh_network.virtual_subnet') }}</label>
                    <InputText id="create-virtual-subnet" v-model="form.virtual_subnet" class="font-mono"
                        :placeholder="t('web.mesh_network.virtual_subnet_default')" :invalid="subnet === null" :disabled="creating" />
                    <small v-if="subnet === null" class="text-red-500">{{ t('web.mesh_network.virtual_subnet_invalid') }}</small>
                    <small v-else class="muted">{{ t('web.mesh_network.virtual_subnet_create_hint') }}</small>
                </div>
                <fieldset class="create-group">
                    <legend>{{ t('web.mesh_network.section_preset') }}</legend>
                    <p class="text-xs muted m-0">{{ t('web.mesh_network.preset_hint') }}</p>
                    <div class="flex items-center gap-2">
                        <label for="create-peers" class="text-sm">{{ t('initial_nodes') }}</label>
                        <span class="pi pi-question-circle text-sm" v-tooltip="t('initial_nodes_help')"></span>
                    </div>
                    <UrlListInput id="create-peers" v-model="peers" :protos="protos" default-url="tcp://:11010"
                        :add-label="t('add_initial_node')" :placeholder="t('initial_node_placeholder')" />
                    <label for="create-listeners" class="text-sm">{{ t('web.mesh_network.listeners') }}</label>
                    <UrlListInput id="create-listeners" v-model="listeners" :protos="protos" default-url="tcp://0.0.0.0:11010"
                        :add-label="t('web.common.add')" placeholder="tcp://0.0.0.0:11010" />
                </fieldset>
                <fieldset class="create-group">
                    <legend>{{ t('web.mesh_network.add_device') }}</legend>
                    <p class="text-xs muted m-0">{{ needsDevices ? t('web.mesh_network.create_devices_required') : t('web.mesh_network.create_devices_optional') }}</p>
                    <div v-if="devices === undefined" class="loading-rows"><Skeleton v-for="i in 2" :key="i" height="2rem" /></div>
                    <div v-else-if="devices.length === 0" class="text-sm muted">{{ t('web.mesh_network.no_online_devices') }}</div>
                    <div v-else class="device-pick-grid">
                        <div v-for="device in devices" :key="device.machine_id" class="device-pick">
                            <Checkbox v-model="selectedDevices" :value="device.machine_id" :input-id="`create-device-${device.machine_id}`" />
                            <label :for="`create-device-${device.machine_id}`" class="min-w-0"><span class="preview-name">{{ device.alias || device.hostname || device.machine_id }}</span>
                                <span class="preview-secondary">{{ t('web.console.instances') }} · {{ device.running_network_count }}</span></label>
                        </div>
                    </div>
                </fieldset>
            </div>
            <template #footer>
                <Button :label="t('web.common.cancel')" icon="pi pi-times" text severity="secondary" @click="createVisible = false" />
                <Button :label="t('web.common.confirm')" icon="pi pi-check" :loading="creating"
                    :disabled="!form.network_name.trim() || nameExists || subnet === null || (needsDevices && !selectedDevices.length)" @click="create" />
            </template>
        </Dialog>
    </div>
</template>

<style scoped>
.page-hint { margin: -8px 0 0; font-size: 13px; color: var(--p-text-muted-color); }
.network-open:hover, .network-open:focus-visible { color: var(--p-primary-color); }
.network-open:focus-visible { outline: 2px solid var(--p-primary-color); outline-offset: 2px; border-radius: 4px; }
.network-facts { grid-template-columns: repeat(3, minmax(0, 1fr)); }
.create-group { display: flex; flex-direction: column; gap: 8px; margin: 0; padding: 10px 12px 12px; border: 1px solid var(--console-border); border-radius: 6px; }
.create-group legend { padding: 0 4px; font-size: 13px; font-weight: 600; }
.required { color: var(--p-red-500); }
.device-pick-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(min(100%, 150px), 1fr)); gap: 8px; max-height: 14rem; overflow: auto; }
.device-pick { display: flex; align-items: center; gap: 8px; min-width: 0; padding: 8px 10px; border: 1px solid var(--console-border); border-radius: 6px; }
.device-pick label { cursor: pointer; }
.device-pick .preview-name { display: block; font-size: 13px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.device-pick .preview-secondary { display: block; font-size: 11px; }
</style>
