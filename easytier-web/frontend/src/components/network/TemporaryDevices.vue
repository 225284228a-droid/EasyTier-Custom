<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue';
import { Button, Column, ConfirmDialog, DataTable, Dialog, Drawer, InputSwitch, InputText, Message, ProgressSpinner, ScrollPanel, Select, SelectButton, Skeleton, Tab, TabList, TabPanel, TabPanels, Tabs, Tag, useConfirm, useToast } from 'primevue';
import { useI18n } from 'vue-i18n';
import { Config, NetworkTypes, UrlListInput, Utils, StatusDisplay } from 'easytier-frontend-lib';
import ApiClient, { type CentralNetworkDetail, type CentralNetworkMember, type NetworkCredential, type NodeAclRuleStat, type NodePeerInfo, type NodeRouteInfo, type TemporaryPeer } from '../../modules/api';
import { normalizeVirtualSubnet } from '../../modules/virtual-subnet';
import { toNetworkCidr } from '../../modules/cidr';
import { DAY, isPermanentExpiry } from '../../modules/credentialTtl';
import CredentialTtlPicker from './CredentialTtlPicker.vue';

// Temporary devices of one network: devices whose whole configuration the
// console publishes, authenticated by credentials instead of the secret.
// The network's name, secret and secure mode follow its core settings.
const { t } = useI18n()
const toast = useToast();
const confirm = useConfirm();

const props = defineProps<{
    api: ApiClient;
    /** The central record that backs the temporary devices. */
    networkId: string;
    /** Devices that already run the network from their own configuration. */
    memberDevices?: string[];
}>();

const api = props.api;

const networkId = computed(() => props.networkId);

const network = ref<CentralNetworkDetail | undefined>(undefined);
const members = ref<CentralNetworkMember[] | undefined>(undefined);
const temporaryPeers = ref<TemporaryPeer[]>([]);
const loadError = ref<string | undefined>(undefined);

const activeTab = ref('members');

const loadAll = async () => {
    try {
        network.value = await api?.get_network(networkId.value);
        loadError.value = undefined;
    } catch (e: any) {
        loadError.value = e?.response?.data?.message ?? String(e);
        return;
    }
    try {
        const view = await api?.list_network_members(networkId.value);
        members.value = view?.members;
        temporaryPeers.value = view?.temporary_peers ?? [];
    } catch (e) {
        loadError.value = String(e);
        console.error(e);
    }
    if (network.value?.secure_mode && credentials.value !== undefined) {
        loadCredentials();
    }
};

const periodFunc = new Utils.PeriodicTask(async () => {
    await loadAll();
}, 2000);

const loadGatewayInfo = async () => {
    if (gatewayInfo.value !== undefined) {
        return;
    }
    try {
        gatewayInfo.value = await api?.get_gateway_info();
    } catch (e) {
        gatewayInfo.value = { enabled: false };
    }
};

onMounted(async () => {
    // Load both in parallel; the settings form must not read a Gateway
    // network before the gateway info is available.
    await Promise.all([loadAll(), loadGatewayInfo()]);
    periodFunc.start();
});

onUnmounted(() => {
    periodFunc.stop();
    window.clearInterval(nodeDetailTimer);
    configRequest++;
    candidateCidrRequest++;
    nodeDetailRequest++;
});

// --- members tab ---

// 成员列表的表格/卡片视图切换，选择持久化；移动端始终使用紧凑列表
const viewMode = ref<'table' | 'card'>(localStorage.getItem('networkMembers.viewMode') === 'table' ? 'table' : 'card');
const viewModeOptions = [
    { value: 'card', icon: 'pi pi-th-large' },
    { value: 'table', icon: 'pi pi-table' },
];
const viewModeLabel = (value: string) => t(value === 'card' ? 'web.console.view_card' : 'web.console.view_table');
watch(viewMode, (mode) => localStorage.setItem('networkMembers.viewMode', mode));

const addMemberVisible = ref(false);
const adding = ref(false);
const candidateDevices = ref<Utils.DeviceInfo[]>([]);
const selectedDevices = ref<Utils.DeviceInfo[]>([]);

// A device joins a network once, either from its own configuration or as a
// temporary device.
const memberDeviceIds = computed(() => new Set([
    ...(members.value ?? []).map((member) => member.device_id),
    ...(props.memberDevices ?? []),
]));

const openAddMember = async () => {
    try {
        const machines = await api?.list_machines() ?? [];
        candidateDevices.value = machines
            .map((machine: any) => Utils.buildDeviceInfo(machine))
            .filter((device: Utils.DeviceInfo) => !!device.machine_id && !memberDeviceIds.value.has(device.machine_id));
    } catch (e) {
        console.error(e);
        candidateDevices.value = [];
    }
    selectedDevices.value = [];
    addTemporary.value = true;
    addTtl.value = 7 * DAY;
    addMemberVisible.value = true;
};

// Permanent members run the network from their own configuration.
const addTemporary = ref(true);
const addTtl = ref(7 * DAY);
// Credential expiries as a date, or permanent.
const expiryDate = (expiryUnix: number) => isPermanentExpiry(expiryUnix)
    ? t('web.network_detail.ttl_permanent') : new Date(expiryUnix * 1000).toLocaleDateString();
const expiryTime = (expiryUnix: number) => isPermanentExpiry(expiryUnix)
    ? t('web.network_detail.ttl_permanent') : new Date(expiryUnix * 1000).toLocaleString();

const addMembers = async () => {
    const deviceIds = selectedDevices.value.map((device) => device.machine_id);
    if (deviceIds.length === 0) {
        return;
    }
    adding.value = true;
    try {
        await api?.add_network_members(
            networkId.value,
            deviceIds,
            addTemporary.value,
            addTemporary.value ? addTtl.value : undefined,
        );
        addMemberVisible.value = false;
        await loadAll();
    } catch (e: any) {
        toast.add({ severity: 'error', summary: t('web.network_detail.add_member'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
    } finally {
        adding.value = false;
    }
};

const removeMember = (member: CentralNetworkMember) => {
    confirm.require({
        header: t('web.network_detail.remove_member'),
        message: t('web.network_detail.remove_member_confirm', { device: member.hostname ?? member.device_id }),
        acceptLabel: t('web.common.confirm'),
        rejectLabel: t('web.common.cancel'),
        accept: async () => {
            try {
                await api?.remove_network_member(networkId.value, member.device_id);
                await loadAll();
            } catch (e: any) {
                toast.add({ severity: 'error', summary: t('web.network_detail.remove_member'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
            }
        },
    });
};

const editMemberVisible = ref(false);
const editingMember = ref<CentralNetworkMember | undefined>(undefined);
const editMemberForm = ref({ hostname_override: '', virtual_ipv4: '' });
const editProxyCidrs = ref<string[]>([]);
const proxyCidrInput = ref('');
const savingMember = ref(false);

// The Enter used to confirm an IME composition must not add the entry.
const onProxyCidrEnter = (event: KeyboardEvent) => {
    if (event.isComposing || event.keyCode === 229) {
        return;
    }
    addProxyCidr();
};

const addProxyCidr = () => {
    const converted = toNetworkCidr(proxyCidrInput.value);
    if (converted === null) {
        toast.add({ severity: 'warn', summary: t('proxy_cidrs'), detail: t('web.network_detail.proxy_cidr_invalid'), life: 3000 });
        return;
    }
    if (!editProxyCidrs.value.includes(converted)) {
        editProxyCidrs.value.push(converted);
    }
    proxyCidrInput.value = '';
};

const openEditMember = (member: CentralNetworkMember) => {
    editingMember.value = member;
    editTab.value = 'basic';
    editMemberForm.value = {
        hostname_override: member.hostname_override ?? '',
        virtual_ipv4: member.virtual_ipv4 ?? '',
    };
    editProxyCidrs.value = [...(member.proxy_cidrs ?? [])];
    proxyCidrInput.value = '';
    candidateCidrs.value = undefined;
    editMemberVisible.value = true;
    loadCandidateCidrs(member);
    loadMemberConfigForm(member);
};

// Suggest subnets detected on the device: each interface IPv4 expands to
// its /24 network; loopback, link-local and the member's own virtual
// subnet are filtered out.
const candidateCidrs = ref<string[] | undefined>(undefined);
let candidateCidrRequest = 0;
const loadCandidateCidrs = async (member: CentralNetworkMember) => {
    const request = ++candidateCidrRequest;
    if (!member.online) {
        candidateCidrs.value = [];
        return;
    }
    try {
        const addrs = await api?.get_node_interface_ips(member.device_id, networkId.value);
        if (request !== candidateCidrRequest) return;
        const virtualPrefix = (member.runtime_virtual_ipv4 ?? member.virtual_ipv4 ?? member.allocated_ipv4 ?? '')
            .split('/')[0].split('.').slice(0, 3).join('.');
        const cidrs = new Set<string>();
        for (const addr of addrs ?? []) {
            const ip = [24, 16, 8, 0].map(shift => (addr >> shift) & 0xff).join('.');
            const first = Number(ip.split('.')[0]);
            if (first === 127 || first === 169 || first === 0 || first === 224) continue;
            const network = ip.split('.').slice(0, 3).join('.') + '.0/24';
            if (virtualPrefix && network.startsWith(virtualPrefix + '.')) continue;
            cidrs.add(network);
        }
        candidateCidrs.value = [...cidrs];
    } catch (e) {
        if (request !== candidateCidrRequest) return;
        console.error(e);
        candidateCidrs.value = [];
    }
};

const toggleCandidateCidr = (cidr: string) => {
    const index = editProxyCidrs.value.indexOf(cidr);
    if (index >= 0) {
        editProxyCidrs.value.splice(index, 1);
    } else {
        editProxyCidrs.value.push(cidr);
    }
};

const saveMember = async () => {
    if (!editingMember.value) {
        return;
    }
    savingMember.value = true;
    try {
        await api?.update_network_member(networkId.value, editingMember.value.device_id, {
            hostname_override: editMemberForm.value.hostname_override.trim(),
            virtual_ipv4: editMemberForm.value.virtual_ipv4.trim(),
            proxy_cidrs: [...editProxyCidrs.value],
        });
        editMemberVisible.value = false;
        await loadAll();
    } catch (e: any) {
        toast.add({ severity: 'error', summary: t('web.network_detail.edit_member'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
    } finally {
        savingMember.value = false;
    }
};

// --- settings tab ---

const settingsForm = ref({
    display_name: '',
    network_name: '',
    network_secret: '',
    virtual_cidr: '',
    secure_mode: false,
});
const settingsNoPassword = ref(false);
// Keep the edit baseline separate from the polled network so unrelated
// settings changes do not overwrite a secret updated by another session.
const settingsSecretBaseline = ref('');
// Clearing an edited secret never removes the password implicitly; only the
// No password checkbox does. Unchanged legacy whitespace secrets stay valid,
// but leaving No password needs a real secret.
const settingsSecretMissing = computed(() =>
    !settingsNoPassword.value
    && settingsForm.value.network_secret.trim() === ''
    && (settingsForm.value.network_secret !== settingsSecretBaseline.value
        || settingsSecretBaseline.value === ''));
// Initial nodes in the same model as the GUI config form; an empty list
// means Standalone.
const initialNodes = ref<string[]>([]);
const settingsLoaded = ref(false);
const savingSettings = ref(false);

const gatewayInfo = ref<{ enabled: boolean; peer_url?: string } | undefined>(undefined);

const protos = NetworkTypes.PEER_PROTOCOLS;

const gatewayFollowed = () =>
    gatewayInfo.value?.enabled === true
    && initialNodes.value.length === 1
    && initialNodes.value[0] === gatewayInfo.value?.peer_url;

const loadSettingsForm = () => {
    if (!network.value) {
        return;
    }
    settingsForm.value = {
        display_name: network.value.display_name,
        network_name: network.value.network_name,
        network_secret: network.value.network_secret,
        virtual_cidr: network.value.virtual_cidr ?? '',
        secure_mode: network.value.secure_mode ?? false,
    };
    settingsNoPassword.value = network.value.network_secret === '';
    settingsSecretBaseline.value = network.value.network_secret;
    // Present every networking method as an initial-node list: the gateway
    // URL for gateway networks, the stored peers otherwise.
    switch (network.value.networking_method) {
        case 'Gateway':
            initialNodes.value =
                gatewayInfo.value?.enabled && gatewayInfo.value?.peer_url
                    ? [gatewayInfo.value.peer_url]
                    : [];
            break;
        case 'Manual':
            initialNodes.value = [...(network.value.peer_urls ?? [])];
            break;
        case 'PublicServer':
            initialNodes.value = network.value.public_server_url
                ? [network.value.public_server_url]
                : [];
            break;
        default:
            initialNodes.value = [];
            break;
    }
    settingsLoaded.value = true;
};

const saveSettings = async () => {
    if (savingSettings.value || settingsSecretMissing.value) return;
    savingSettings.value = true;
    try {
        // PublicServer retains its discovery mode while it has a single URL.
        // Keeping exactly the gateway URL as the only initial node preserves
        // the Gateway mode (the compiled peer list follows the gateway URL);
        // anything else compiles to Manual peers, or Standalone when empty.
        const method = network.value?.networking_method === 'PublicServer' && initialNodes.value.length === 1 ? 'PublicServer'
            : gatewayFollowed() ? 'Gateway'
            : initialNodes.value.length > 0 ? 'Manual' : 'Standalone';
        const settings = {
            display_name: settingsForm.value.display_name,
            network_name: (settingsForm.value.network_name ?? '').trim() || null,
            networking_method: method,
            public_server_url: method === 'PublicServer' ? initialNodes.value[0] : null,
            peer_urls: method === 'Manual' ? initialNodes.value : [],
            virtual_cidr: normalizeVirtualSubnet(settingsForm.value.virtual_cidr) || null,
            secure_mode: settingsForm.value.secure_mode,
        };
        const secretWasEdited = settingsForm.value.network_secret !== settingsSecretBaseline.value;
        let secret: string | undefined;
        if (settingsNoPassword.value) {
            settingsForm.value.network_secret = '';
            if (settingsSecretBaseline.value !== '') {
                secret = '';
            }
        } else if (secretWasEdited) {
            const trimmedSecret = settingsForm.value.network_secret.trim();
            settingsForm.value.network_secret = trimmedSecret;
            if (trimmedSecret !== settingsSecretBaseline.value) {
                secret = trimmedSecret;
            }
        }
        const updated = await api?.update_network(networkId.value, settings, secret);
        if (updated) {
            settingsForm.value.network_secret = updated.network_secret;
            settingsNoPassword.value = updated.network_secret === '';
            settingsSecretBaseline.value = updated.network_secret;
        }
        await loadAll();
        toast.add({ severity: 'success', summary: t('web.network_detail.settings'), detail: t('web.device_management.config_saved'), life: 2000 });
    } catch (e: any) {
        toast.add({ severity: 'error', summary: t('web.network_detail.settings'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
    } finally {
        savingSettings.value = false;
    }
};

// --- member advanced config (advanced tab of the edit dialog) ---

const editTab = ref<'basic' | 'advanced'>('basic');
const configSaving = ref(false);
const configLoaded = ref(false);
const configMember = ref<CentralNetworkMember | null>(null);
const configForm = ref<NetworkTypes.NetworkConfig>(NetworkTypes.DEFAULT_NETWORK_CONFIG());
const configCapabilities = ref<string[]>([]);
const configBaseline = ref<{ network_name: string; network_secret: string }>({ network_name: '', network_secret: '' });
let configRequest = 0;

watch(editMemberVisible, (visible) => {
    if (!visible) {
        configRequest++;
        candidateCidrRequest++;
    }
}, { flush: 'sync' });

const loadMemberConfigForm = async (member: CentralNetworkMember) => {
    const request = ++configRequest;
    configMember.value = member;
    configLoaded.value = false;
    try {
        const [raw, capabilities] = await Promise.all([
            api?.get_member_config(networkId.value, member.device_id),
            api?.runtime_capabilities(member.device_id),
        ]);
        if (request !== configRequest) return;
        configCapabilities.value = capabilities ?? [];
        configForm.value = NetworkTypes.normalizeNetworkConfig(raw);
        configBaseline.value = {
            network_name: String(configForm.value.network_name ?? ''),
            network_secret: String(configForm.value.network_secret ?? ''),
        };
        configLoaded.value = true;
    } catch (e: any) {
        if (request !== configRequest) return;
        toast.add({ severity: 'error', summary: t('web.network_detail.advanced_config'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
    }
};

const saveMemberConfig = async () => {
    if (!configMember.value) return;
    const form = configForm.value;
    if (String(form.network_name ?? '') !== configBaseline.value.network_name
        || String(form.network_secret ?? '') !== configBaseline.value.network_secret) {
        toast.add({ severity: 'warn', summary: t('web.network_detail.advanced_config'),
            detail: t('web.network_detail.identity_ignored'), life: 4000 });
    }
    configSaving.value = true;
    try {
        await api?.set_member_config(networkId.value, configMember.value.device_id, NetworkTypes.toBackendNetworkConfig(form));
        editMemberVisible.value = false;
        await loadAll();
        toast.add({ severity: 'success', summary: t('web.network_detail.advanced_config'), detail: t('web.device_management.config_saved'), life: 2000 });
    } catch (e: any) {
        toast.add({ severity: 'error', summary: t('web.network_detail.advanced_config'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
    } finally {
        configSaving.value = false;
    }
};

const resetMemberConfig = async () => {
    if (!configMember.value) return;
    configSaving.value = true;
    try {
        await api?.clear_member_config(networkId.value, configMember.value.device_id);
        editMemberVisible.value = false;
        await loadAll();
        toast.add({ severity: 'success', summary: t('web.network_detail.advanced_config'), detail: t('web.network_detail.config_reset'), life: 2000 });
    } catch (e: any) {
        toast.add({ severity: 'error', summary: t('web.network_detail.advanced_config'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
    } finally {
        configSaving.value = false;
    }
};

// --- node detail dialog (runtime view of one member in this network) ---

const nodeDetailVisible = ref(false);
const nodeLoggerLevel = ref<number | undefined>(undefined);
const nodeLoggerSaving = ref(false);
const nodeTomlConfig = ref<string | undefined>(undefined);
const nodeConfigVisible = ref(false);
// Shared generation of the node-detail dialog: every open, close and unmount
// invalidates the async requests of the previous device, so a slow response
// can never write another device's data into the current view.
let nodeDetailRequest = 0;

const loggerLevelOptions = computed(() => [
    { label: t('web.network_detail.log_disabled'), value: 0 },
    { label: t('web.network_detail.log_error'), value: 1 },
    { label: t('web.network_detail.log_warning'), value: 2 },
    { label: t('web.network_detail.log_info'), value: 3 },
    { label: t('web.network_detail.log_debug'), value: 4 },
    { label: t('web.network_detail.log_trace'), value: 5 },
]);

const loadNodeGeneral = async () => {
    if (!nodeDetailMember.value) return;
    const request = nodeDetailRequest;
    const device_id = nodeDetailMember.value.device_id;
    try {
        const level = await api?.get_node_logger_level(device_id);
        if (request !== nodeDetailRequest) return;
        nodeLoggerLevel.value = level;
    } catch (e) {
        if (request !== nodeDetailRequest) return;
        console.error(e);
    }
};

const applyNodeLoggerLevel = async (level: number) => {
    if (!nodeDetailMember.value) return;
    const request = nodeDetailRequest;
    const device_id = nodeDetailMember.value.device_id;
    nodeLoggerSaving.value = true;
    try {
        await api?.set_node_logger_level(device_id, level);
        if (request !== nodeDetailRequest || nodeDetailMember.value?.device_id !== device_id) return;
        nodeLoggerLevel.value = level;
        toast.add({ severity: 'success', summary: t('web.network_detail.log_level'), detail: t('web.device_management.config_saved'), life: 2000 });
    } catch (e: any) {
        if (request !== nodeDetailRequest) return;
        toast.add({ severity: 'error', summary: t('web.network_detail.log_level'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
    } finally {
        nodeLoggerSaving.value = false;
    }
};

const exportNodeConfig = async () => {
    if (!nodeDetailMember.value) return;
    const request = nodeDetailRequest;
    const device_id = nodeDetailMember.value.device_id;
    nodeTomlConfig.value = undefined;
    nodeConfigVisible.value = true;
    try {
        const config = await api?.get_node_toml_config(device_id, networkId.value);
        if (request !== nodeDetailRequest || nodeDetailMember.value?.device_id !== device_id) return;
        nodeTomlConfig.value = config;
    } catch (e: any) {
        if (request !== nodeDetailRequest) return;
        nodeTomlConfig.value = '';
        toast.add({ severity: 'error', summary: t('web.network_detail.export_config'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
    }
};

const copyNodeConfig = async () => {
    if (!nodeTomlConfig.value) return;
    try {
        await navigator.clipboard.writeText(nodeTomlConfig.value);
        toast.add({ severity: 'success', summary: t('web.network_detail.export_config'), detail: t('copy_config'), life: 2000 });
    } catch {
        toast.add({ severity: 'error', summary: t('web.network_detail.export_config'), detail: 'clipboard unavailable', life: 3000 });
    }
};

const downloadNodeConfig = () => {
    if (!nodeTomlConfig.value || !nodeDetailMember.value) return;
    const blob = new Blob([nodeTomlConfig.value], { type: 'text/plain' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `${nodeDetailMember.value.hostname ?? nodeDetailMember.value.device_id}.toml`;
    a.click();
    URL.revokeObjectURL(url);
};

const nodeDetailMember = ref<CentralNetworkMember | null>(null);
const nodeRoutes = ref<NodeRouteInfo[] | undefined>(undefined);
const nodePeers = ref<NodePeerInfo[] | undefined>(undefined);
const nodeAclStats = ref<NodeAclRuleStat[] | undefined>(undefined);
const nodeDetailError = ref<string | undefined>(undefined);

const ipv4ToString = (addr?: { address?: { addr?: number } } | null) => {
    const value = addr?.address?.addr;
    return value != null
        ? [24, 16, 8, 0].map(shift => (value >> shift) & 0xff).join('.')
        : '';
};

const loadNodeDetail = async () => {
    if (!nodeDetailMember.value) return;
    const request = nodeDetailRequest;
    const machine_id = nodeDetailMember.value.device_id;
    const inst_id = networkId.value;
    nodeDetailError.value = undefined;
    const results = await Promise.allSettled([
        api?.get_node_routes(machine_id, inst_id),
        api?.get_node_peers(machine_id, inst_id),
        api?.get_node_acl_stats(machine_id, inst_id),
    ]);
    if (request !== nodeDetailRequest || nodeDetailMember.value?.device_id !== machine_id) return;
    if (results[0].status === 'fulfilled') nodeRoutes.value = results[0].value ?? [];
    if (results[1].status === 'fulfilled') nodePeers.value = results[1].value ?? [];
    if (results[2].status === 'fulfilled') nodeAclStats.value = results[2].value ?? [];
    nodeDetailError.value = results.find(r => r.status === 'rejected')
        ? String((results.find(r => r.status === 'rejected') as PromiseRejectedResult)?.reason?.response?.data?.message
            ?? (results.find(r => r.status === 'rejected') as PromiseRejectedResult)?.reason)
        : undefined;
};

let nodeDetailTimer: number | undefined;
const openNodeDetail = (member: CentralNetworkMember) => {
    nodeDetailMember.value = member;
    nodeRoutes.value = undefined;
    nodePeers.value = undefined;
    nodeAclStats.value = undefined;
    nodeDetailVisible.value = true;
    nodeLoggerLevel.value = undefined;
    nodeDetailRequest++;
    loadNodeDetail();
    loadNodeGeneral();
    window.clearInterval(nodeDetailTimer);
    nodeDetailTimer = window.setInterval(() => {
        if (!nodeDetailVisible.value) return;
        loadNodeDetail();
    }, 5_000);
};
const closeNodeDetail = () => {
    window.clearInterval(nodeDetailTimer);
    nodeDetailTimer = undefined;
    nodeDetailVisible.value = false;
    nodeDetailRequest++;
};

const routeCostLabel = (cost: number) => cost === 1 ? 'p2p' : `relay(${cost})`;
const latencyLabel = (latencyMs: number | undefined) =>
    latencyMs == null ? '—' : `${Math.round(latencyMs)} ms`;
const peerTelemetry = (peer: NodePeerInfo) => {
    const info = { peer } as unknown as NetworkTypes.PeerRoutePair;
    return { bandwidth: StatusDisplay.estimatedBandwidth(info), traffic: StatusDisplay.totalTraffic(info) };
};
const connectionBandwidth = (conn: NodePeerInfo['conns'][number]) =>
    StatusDisplay.estimatedConnectionBandwidth(conn as unknown as NetworkTypes.PeerConnInfo);
const connectionLatency = (conn: NodePeerInfo['conns'][number]) => {
    const microseconds = StatusDisplay.numericValue(conn.stats?.latency_us);
    return microseconds === undefined ? undefined : microseconds / 1000;
};

// One row per known peer: route facts (hostname / IP / hop cost) merged
// with its direct connections' protocols and best latency. Peers seen only
// in the route table (no direct conn) still show up with relay costs.
const nodePeerRows = computed(() => {
    const connsByPeer = new Map<number, NodePeerInfo>();
    (nodePeers.value ?? []).forEach(peer => connsByPeer.set(peer.peer_id, peer));
    const rows = new Map<number, { key: string; peer_id: number; hostname: string; ipv4: string; cost: number; protocols: string[]; latencyMs?: number; bandwidth: { upload: string; download: string }; traffic: { upload: string; download: string } }>();
    (nodeRoutes.value ?? []).forEach(route => {
        const addr = route.ipv4_addr;
        const ip = ipv4ToString(addr);
        rows.set(route.peer_id, {
            key: String(route.peer_id),
            peer_id: route.peer_id,
            hostname: route.hostname,
            ipv4: ip ? `${ip}/${addr?.network_length ?? 24}` : '',
            cost: route.cost,
            protocols: [],
            latencyMs: route.path_latency,
            bandwidth: { upload: '--', download: '--' },
            traffic: { upload: '--', download: '--' },
        });
    });
    connsByPeer.forEach((peer, peer_id) => {
        const conns = peer.conns ?? [];
        const protocols = [...new Set(conns.map(conn => conn.tunnel?.tunnel_type).filter((v): v is string => !!v))];
        const bestLatency = conns
            .map(conn => StatusDisplay.numericValue(conn.stats?.latency_us))
            .map(value => value === undefined ? undefined : value / 1000)
            .filter((v): v is number => v != null)
            .sort((a, b) => a - b)[0];
        const row = rows.get(peer_id) ?? {
            key: String(peer_id),
            peer_id,
            hostname: '',
            ipv4: '',
            cost: 1,
            protocols: [],
            latencyMs: undefined,
            bandwidth: { upload: '--', download: '--' },
            traffic: { upload: '--', download: '--' },
        };
        Object.assign(row, peerTelemetry(peer));
        row.protocols = protocols;
        if (bestLatency != null && (row.latencyMs == null || bestLatency < row.latencyMs)) {
            row.latencyMs = bestLatency;
        }
        rows.set(peer_id, row);
    });
    return [...rows.values()].sort((a, b) => a.hostname.localeCompare(b.hostname));
});

// flatten peer connections for the connection table
const nodeConnRows = computed(() => {
    const hostnameByPeer = new Map<number, string>();
    (nodeRoutes.value ?? []).forEach(route => hostnameByPeer.set(route.peer_id, route.hostname));
    const rows: Array<{ conn: NodePeerInfo['conns'][number], hostname: string }> = [];
    (nodePeers.value ?? []).forEach(peer => {
        (peer.conns ?? []).forEach(conn => {
            rows.push({ conn, hostname: hostnameByPeer.get(peer.peer_id) ?? `#${peer.peer_id}` });
        });
    });
    return rows;
});

// --- credentials tab ---

const credentials = ref<NetworkCredential[] | undefined>(undefined);

const loadCredentials = async () => {
    if (!network.value?.secure_mode) return;
    try {
        credentials.value = await api?.list_credentials(networkId.value);
    } catch (e) {
        console.error(e);
    }
};

const highlightedCredential = ref<string | null>(null);

const locateCredential = async (credentialId: string) => {
    highlightedCredential.value = credentialId;
    activeTab.value = 'credentials';
    await switchTabWithCredentials('credentials');
    await nextTick();
    document
        .querySelector('.credential-row-highlight')
        ?.scrollIntoView({ behavior: 'smooth', block: 'center' });
};

const switchTabWithCredentials = async (tab: string) => {
    if (tab === 'credentials' && credentials.value === undefined) {
        await loadCredentials();
    }
};

const generateVisible = ref(false);
const generating = ref(false);
const generateForm = ref({ ttl: DAY, reusable: true });
const generatedCredential = ref<NetworkCredential | null>(null);

const openGenerateCredential = () => {
    generateForm.value = { ttl: DAY, reusable: true };
    generatedCredential.value = null;
    generateVisible.value = true;
};

const generateCredential = async () => {
    generating.value = true;
    try {
        const created = await api?.generate_credential(
            networkId.value,
            generateForm.value.ttl,
            generateForm.value.reusable,
        );
        if (created) {
            generatedCredential.value = created;
        }
        await loadCredentials();
    } catch (e: any) {
        toast.add({ severity: 'error', summary: t('web.network_detail.generate_credential'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
    } finally {
        generating.value = false;
    }
};

const revokeCredential = (credential: NetworkCredential) => {
    confirm.require({
        header: t('web.network_detail.revoke_credential'),
        message: t('web.network_detail.revoke_credential_confirm', { id: credential.credential_id }),
        acceptLabel: t('web.common.confirm'),
        rejectLabel: t('web.common.cancel'),
        accept: async () => {
            try {
                await api?.revoke_credential(networkId.value, credential.credential_id);
                await loadCredentials();
                revealedSecrets.value.delete(credential.credential_id);
            } catch (e: any) {
                toast.add({ severity: 'error', summary: t('web.network_detail.revoke_credential'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
            }
        },
    });
};

// --- credential secret reveal & join command viewer ---

const revealedSecrets = ref(new Set<string>());

const toggleSecretRevealed = (credential: NetworkCredential) => {
    const next = new Set(revealedSecrets.value);
    if (next.has(credential.credential_id)) {
        next.delete(credential.credential_id);
    } else {
        next.add(credential.credential_id);
    }
    revealedSecrets.value = next;
};

const joinCommandCredential = ref<NetworkCredential | null>(null);

const showJoinCommand = (credential: NetworkCredential) => {
    joinCommandCredential.value = credential;
};

const credentialJoinPeer = () => {
    if (!network.value) return '';
    if (network.value.networking_method === 'PublicServer') return network.value.public_server_url ?? '';
    return (network.value.networking_method === 'Gateway' && gatewayInfo.value?.peer_url)
        ? gatewayInfo.value.peer_url
        : (network.value.peer_urls ?? [])[0] ?? '';
};

const credentialJoinCommand = (secret: string) => {
    if (!network.value) return '';
    const peer = credentialJoinPeer();
    return [
        'easytier-core',
        `--network-name ${network.value.network_name}`,
        '--secure-mode',
        `--credential ${secret}`,
        peer ? `-p ${peer}` : '',
    ].filter(Boolean).join(' ');
};

// Config-file form of the same join: the credential secret doubles as the
// node's x25519 private key; the matching public key is derived on load.
const credentialJoinToml = (secret: string) => {
    if (!network.value) return '';
    const peer = credentialJoinPeer();
    return [
        '[network_identity]',
        `network_name = "${network.value.network_name}"`,
        '',
        '[[peer]]',
        `uri = "${peer}"`,
        '',
        '[secure_mode]',
        'enabled = true',
        `local_private_key = "${secret}"`,
    ].join('\n');
};

type JoinFormat = 'cli' | 'toml';
const joinFormat = ref<JoinFormat>('cli');
const joinFormatOptions = [
    { label: t('web.network_detail.credential_join_cli'), value: 'cli' },
    { label: t('web.network_detail.credential_join_toml'), value: 'toml' },
];
const joinText = (secret: string) =>
    joinFormat.value === 'cli' ? credentialJoinCommand(secret) : credentialJoinToml(secret);

const copyText = async (text: string, what: string) => {
    let copied = false;
    if (navigator.clipboard?.writeText) {
        try {
            await navigator.clipboard.writeText(text);
            copied = true;
        } catch {
            // Fall through to the legacy path below.
        }
    }
    if (!copied) {
        // Plain-HTTP deployments have no async clipboard API; the legacy
        // synchronous copy still works inside the click gesture.
        const textarea = document.createElement('textarea');
        textarea.value = text;
        textarea.style.position = 'fixed';
        textarea.style.opacity = '0';
        document.body.appendChild(textarea);
        textarea.select();
        try {
            copied = document.execCommand('copy');
        } catch {
            copied = false;
        }
        document.body.removeChild(textarea);
    }
    if (copied) {
        toast.add({ severity: 'success', summary: what, detail: t('copy_config'), life: 2000 });
    } else {
        toast.add({ severity: 'error', summary: what, detail: t('web.network_detail.copy_failed'), life: 4000 });
    }
};

const switchTab = async (tab: string) => {
    activeTab.value = tab;
    await switchTabWithCredentials(tab);
    if (tab === 'settings' && !settingsLoaded.value) {
        // Guard the Gateway conversion against the gateway info not having
        // arrived yet; otherwise a Gateway network would load as an empty
        // list and saving would silently downgrade it to Standalone.
        await loadGatewayInfo();
        loadSettingsForm();
    }
};
</script>

<template>
    <div class="temporary-devices">
        <ConfirmDialog />
        <div v-if="loadError !== undefined">
            <Message severity="warn" :closable="false">{{ t('web.console.load_failed') }} <Button :label="t('web.console.retry')" text size="small" @click="loadAll" /></Message>
        </div>

        <div v-if="network === undefined && !loadError" class="console-panel loading-rows">
            <Skeleton v-for="i in 4" :key="i" height="2rem" />
        </div>

        <Tabs v-if="network !== undefined" :value="activeTab" @update:value="switchTab(String($event))" lazy class="console-panel network-tabs">
            <TabList>
                <Tab value="members">{{ t('web.network_detail.tab_members') }}</Tab>
                <Tab v-if="network.secure_mode" value="credentials">{{ t('web.network_detail.tab_credentials') }}</Tab>
                <Tab value="settings">{{ t('web.mesh_network.temporary_settings') }}</Tab>
            </TabList>
            <TabPanels>
            <!-- members -->
            <TabPanel value="members" class="flex flex-col gap-3">
                <div class="flex flex-wrap justify-between items-center gap-3">
                    <div class="text-sm muted flex-1 min-w-48">{{ t('web.network_detail.members_hint') }}</div>
                    <div class="flex items-center gap-3 shrink-0">
                        <SelectButton v-model="viewMode" :options="viewModeOptions" optionValue="value"
                            :allow-empty="false" size="small" class="view-toggle">
                            <template #option="{ option }">
                                <i :class="option.icon" v-tooltip.top="viewModeLabel(option.value)"
                                    :aria-label="viewModeLabel(option.value)"></i>
                            </template>
                        </SelectButton>
                        <Button :label="t('web.network_detail.add_member')" icon="pi pi-plus" size="small" class="shrink-0" @click="openAddMember" />
                    </div>
                </div>
                <div v-if="members === undefined" class="w-full flex justify-center py-4"><ProgressSpinner /></div>
                <div v-else-if="members.length === 0" class="text-center p-6 text-500">{{ t('web.console.no_results') }}</div>
                <template v-else>
                    <DataTable v-if="viewMode === 'table'" :value="members" dataKey="device_id" size="small" class="console-table desktop-list" scrollable>
                        <Column :header="t('web.device.hostname')">
                            <template #body="{ data }">
                                <div class="entity-link">
                                    <span class="status-dot" :class="data.online ? 'online' : 'offline'"
                                        v-tooltip.top="data.online ? t('web.device.online') : t('web.device.offline')"></span>
                                    <div class="min-w-0">
                                        <div class="preview-name">{{ data.hostname_override || data.alias || data.hostname || data.device_id }}</div>
                                        <div v-if="data.hostname_override" class="preview-secondary">{{ data.hostname_override }}</div>
                                        <div v-else-if="data.alias && data.hostname" class="preview-secondary">{{ data.hostname }}</div>
                                    </div>
                                </div>
                            </template>
                        </Column>
                        <Column :header="t('web.network_detail.static_ip')">
                            <template #body="{ data }">
                                <span v-if="data.runtime_virtual_ipv4" class="mono-value">{{ data.runtime_virtual_ipv4 }}</span>
                                <span v-else-if="data.virtual_ipv4 || data.allocated_ipv4" class="mono-value muted">{{ data.virtual_ipv4 ?? data.allocated_ipv4 }}/{{ network?.virtual_cidr?.split('/')[1] ?? 24 }}</span>
                                <span v-else class="muted">{{ t('virtual_ipv4_dhcp') }}</span>
                            </template>
                        </Column>
                        <Column :header="t('web.device.version')" style="width: 6rem">
                            <template #body="{ data }"><span class="muted">v{{ data.version?.split('-')[0] ?? '?' }}</span></template>
                        </Column>
                        <Column :header="t('web.device.status')" style="width: 8rem">
                            <template #body="{ data }">
                                <Tag v-if="data.temporary" severity="warn" :value="t('web.network_detail.temporary_tag')" />
                                <Tag v-else-if="data.running === true" severity="success" :value="t('web.network_detail.running')" />
                                <Tag v-else-if="data.running === false" severity="secondary" :value="t('web.network_detail.stopped')" />
                                <span v-else class="muted">—</span>
                            </template>
                        </Column>
                        <Column :header="t('web.network_detail.member_error')">
                            <template #body="{ data }">
                                <span v-if="data.error_msg" class="text-red-500 truncate" :title="data.error_msg">{{ data.error_msg }}</span>
                                <span v-else class="muted">—</span>
                            </template>
                        </Column>
                        <Column :header="t('web.console.manage')" style="width: 8rem">
                            <template #body="{ data }">
                                <div class="flex items-center gap-1">
                                    <i v-if="data.has_override" class="pi pi-sliders-h override-marker"
                                        v-tooltip.top="t('web.network_detail.has_override')"></i>
                                    <Button icon="pi pi-chart-line" text rounded severity="secondary"
                                        :disabled="!data.online" :aria-label="t('web.network_detail.node_detail')"
                                        v-tooltip.top="data.online ? t('web.network_detail.node_detail') : t('web.network_detail.node_detail_offline')"
                                        @click="openNodeDetail(data)" />
                                    <Button icon="pi pi-pencil" text rounded severity="secondary" :aria-label="t('web.network_detail.edit_member')"
                                        v-tooltip.top="t('web.network_detail.edit_member')"
                                        @click="openEditMember(data)" />
                                    <Button icon="pi pi-trash" text rounded severity="danger" :aria-label="t('web.network_detail.remove_member')"
                                        @click="removeMember(data)" />
                                </div>
                            </template>
                        </Column>
                    </DataTable>
                    <div class="mobile-list" :class="{ 'member-card-grid': viewMode === 'card' }">
                        <div v-for="member in members" :key="member.device_id" class="member-card"
                            :class="{ offline: !member.online }">
                            <div class="member-title-row">
                                <span class="status-dot" :class="member.online ? 'online' : 'offline'"
                                    v-tooltip.top="member.online ? t('web.device.online') : t('web.device.offline')"></span>
                                <div class="font-semibold truncate" :title="(member.hostname_override || member.alias || member.hostname) ?? member.device_id">
                                    {{ (member.hostname_override || member.alias || member.hostname) ?? member.device_id }}
                                </div>
                            </div>
                            <div class="text-sm flex flex-col gap-0.5 min-w-0 mt-1.5">
                                <div class="flex items-center gap-1.5 flex-wrap">
                                    <Tag v-if="member.temporary" :value="t('web.network_detail.temporary_tag')" severity="secondary" />
                                    <span v-if="member.temporary && member.credential_expiry_unix"
                                        class="text-xs muted">{{ t('web.network_detail.expires_at') }}
                                        {{ expiryDate(member.credential_expiry_unix) }}</span>
                                    <Tag v-if="member.running === true" :value="t('web.network_detail.running')" severity="success" />
                                    <Tag v-else-if="member.running === false" :value="t('web.network_detail.stopped')" severity="warn" />
                                    <span v-if="member.runtime_virtual_ipv4" class="font-mono">{{ member.runtime_virtual_ipv4 }}</span>
                                    <span v-else-if="member.virtual_ipv4 || member.allocated_ipv4" class="font-mono muted">{{ member.virtual_ipv4 ?? member.allocated_ipv4 }}/{{ network?.virtual_cidr?.split('/')[1] ?? 24 }}</span>
                                    <span v-else class="text-500">{{ t('virtual_ipv4_dhcp') }}</span>
                                    <span v-if="member.version" class="text-xs muted">v{{ member.version.split('-')[0] }}</span>
                                </div>
                                <span v-if="member.hostname_override" class="text-xs muted truncate"
                                    :title="t('web.network_detail.hostname_override')">{{ member.hostname_override }}</span>
                                <span v-if="member.error_msg" class="text-xs text-red-500 break-words" :title="member.error_msg">{{ member.error_msg }}</span>
                            </div>
                            <div class="member-actions">
                                <span v-if="member.has_override" class="pi pi-sliders override-marker"
                                    v-tooltip.top="t('web.network_detail.has_override')"></span>
                                <Button icon="pi pi-chart-line" text rounded severity="secondary"
                                    :disabled="!member.online" :aria-label="t('web.network_detail.node_detail')"
                                    v-tooltip.top="member.online ? t('web.network_detail.node_detail') : t('web.network_detail.node_detail_offline')"
                                    @click="openNodeDetail(member)" />
                                <Button icon="pi pi-pencil" text rounded severity="secondary" :aria-label="t('web.network_detail.edit_member')"
                                    v-tooltip.top="t('web.network_detail.edit_member')"
                                    @click="openEditMember(member)" />
                                <Button icon="pi pi-trash" text rounded severity="danger" :aria-label="t('web.network_detail.remove_member')"
                                    @click="removeMember(member)" />
                            </div>
                        </div>
                    </div>
                </template>
                <div v-if="temporaryPeers.length > 0" class="flex flex-col gap-2">
                    <div class="text-sm font-semibold flex items-center gap-2">
                        <i class="pi pi-clock text-xs"></i>
                        {{ t('web.network_detail.temporary_devices') }}
                        <span class="text-xs font-normal muted">{{ t('web.network_detail.temporary_devices_hint') }}</span>
                    </div>
                    <DataTable :value="temporaryPeers" size="small" class="console-table desktop-list" scrollable>
                        <Column :header="t('web.device.hostname')">
                            <template #body="{ data }">
                                <div class="entity-link">
                                    <span class="status-dot online" v-tooltip.top="t('web.device.online')"></span>
                                    <span class="preview-name">{{ data.hostname ?? t('web.network_detail.temporary_device') }}</span>
                                    <Tag severity="warn" :value="t('web.network_detail.temporary_tag')" class="shrink-0" />
                                </div>
                            </template>
                        </Column>
                        <Column :header="t('web.network_detail.static_ip')">
                            <template #body="{ data }"><span class="mono-value">{{ data.ipv4 ?? '—' }}</span></template>
                        </Column>
                        <Column :header="t('web.device.version')" style="width: 6rem">
                            <template #body="{ data }"><span class="muted">v{{ data.version?.split('-')[0] ?? '?' }}</span></template>
                        </Column>
                        <Column :header="t('web.network_detail.temporary_credential')">
                            <template #body="{ data }">
                                <span v-if="data.credential_id" class="mono-value muted" :title="data.credential_id">{{ data.credential_id.slice(0, 13) }}…</span>
                                <span v-else class="muted truncate" :title="t('web.network_detail.temporary_credential_unknown')">{{ t('web.network_detail.temporary_credential_unknown') }}</span>
                            </template>
                        </Column>
                        <Column :header="t('web.network_detail.credential_expiry')" style="width: 8rem">
                            <template #body="{ data }">
                                <span v-if="data.credential_expiry_unix" class="muted">{{ expiryDate(data.credential_expiry_unix) }}</span>
                                <span v-else class="muted">—</span>
                            </template>
                        </Column>
                        <Column :header="t('web.console.manage')" style="width: 8rem">
                            <template #body="{ data }">
                                <Button v-if="data.credential_id" :label="t('web.network_detail.locate_credential')" text size="small" severity="secondary"
                                    @click="locateCredential(data.credential_id)" />
                            </template>
                        </Column>
                    </DataTable>
                    <div class="mobile-list">
                        <div v-for="peer in temporaryPeers" :key="peer.peer_id" class="member-card">
                            <div class="member-title-row">
                                <span class="status-dot online"
                                    v-tooltip.top="t('web.device.online')"></span>
                                <div class="font-semibold truncate" :title="peer.hostname ?? t('web.network_detail.temporary_device')">
                                    {{ peer.hostname ?? t('web.network_detail.temporary_device') }}
                                </div>
                                <Tag :value="t('web.network_detail.temporary_tag')" severity="secondary" class="ml-1 shrink-0" />
                            </div>
                            <div class="text-sm flex items-center gap-1.5 flex-wrap mt-1.5">
                                <span v-if="peer.ipv4" class="font-mono">{{ peer.ipv4 }}</span>
                                <span v-if="peer.version" class="text-xs muted">v{{ peer.version.split('-')[0] }}</span>
                                <template v-if="peer.credential_id">
                                    <span class="text-xs muted truncate" :title="peer.credential_id">
                                        {{ peer.credential_id.slice(0, 13) }}…
                                    </span>
                                    <span v-if="peer.credential_expiry_unix" class="text-xs muted">
                                        · {{ t('web.network_detail.expires_at') }}
                                        {{ expiryDate(peer.credential_expiry_unix) }}
                                    </span>
                                    <Button :label="t('web.network_detail.locate_credential')" text size="small" severity="secondary"
                                        @click="locateCredential(peer.credential_id!)" />
                                </template>
                                <span v-else class="text-xs muted truncate" :title="t('web.network_detail.temporary_credential_unknown')">
                                    {{ t('web.network_detail.temporary_credential_unknown') }}
                                </span>
                            </div>
                        </div>
                    </div>
                </div>
            </TabPanel>

            <!-- credentials -->
            <TabPanel value="credentials" class="flex flex-col gap-3">
                <div class="flex flex-wrap justify-between items-center gap-3">
                    <div class="text-sm muted flex-1 min-w-48">{{ t('web.network_detail.credentials_hint') }}</div>
                    <Button :label="t('web.network_detail.generate_credential')" icon="pi pi-key" size="small" class="shrink-0"
                        @click="openGenerateCredential" />
                </div>
                <DataTable :value="credentials ?? []" dataKey="credential_id" size="small" class="console-table" scrollable tableStyle="min-width: 50rem"
                    :rowClass="(data: any) => data.credential_id === highlightedCredential ? 'credential-row-highlight' : ''">
                    <Column field="credential_id" :header="t('web.network_detail.credential_id')" />
                    <Column :header="t('web.network_detail.credential_secret')"><template #body="{ data }">
                        <div class="flex items-center gap-1.5">
                            <span v-if="revealedSecrets.has(data.credential_id)" class="font-mono text-xs break-all">{{ data.credential_secret }}</span>
                            <span v-else class="font-mono text-xs select-none">••••••••••••••••••••••</span>
                            <Button :icon="revealedSecrets.has(data.credential_id) ? 'pi pi-eye-slash' : 'pi pi-eye'" text rounded severity="secondary"
                                :aria-label="t('web.network_detail.credential_secret')"
                                v-tooltip.top="revealedSecrets.has(data.credential_id) ? t('web.network_detail.credential_hide_secret') : t('web.network_detail.credential_show_secret')"
                                @click="toggleSecretRevealed(data)" />
                            <Button icon="pi pi-copy" text rounded severity="secondary"
                                :aria-label="t('web.network_detail.credential_copy_secret')"
                                v-tooltip.top="t('web.network_detail.credential_copy_secret')"
                                @click="copyText(data.credential_secret, t('web.network_detail.credential_secret'))" />
                        </div>
                    </template></Column>
                    <Column :header="t('web.network_detail.credential_expiry')"><template #body="{ data }">
                        {{ expiryTime(data.expiry_unix) }}
                    </template></Column>
                    <Column :header="t('web.network_detail.credential_online_devices')"><template #body="{ data }">
                        <span v-if="!data.online_peers?.length" class="text-500">—</span>
                        <div v-else class="flex flex-col gap-0.5">
                            <span v-for="peer in data.online_peers" :key="peer.peer_id"
                                class="text-sm truncate">
                                {{ peer.hostname ?? t('web.network_detail.temporary_device') }}<span v-if="peer.ipv4" class="font-mono text-xs muted">&nbsp;{{ peer.ipv4 }}</span>
                            </span>
                        </div>
                    </template></Column>
                    <Column :header="t('web.network_detail.credential_status')"><template #body="{ data }">
                        <Tag :value="data.expiry_unix > Date.now() / 1000 ? t('web.network_detail.credential_active') : t('web.network_detail.credential_expired')"
                            :severity="data.expiry_unix > Date.now() / 1000 ? 'success' : 'danger'" />
                    </template></Column>
                    <Column :header="t('web.network_detail.credential_reusable')"><template #body="{ data }">
                        {{ data.reusable ? t('web.common.confirm') : '—' }}
                    </template></Column>
                    <Column :header="t('web.console.manage')"><template #body="{ data }">
                        <div class="flex items-center gap-1">
                            <Button :label="t('web.network_detail.credential_show_command')" text size="small" severity="secondary"
                                @click="showJoinCommand(data)" />
                            <Button :label="t('web.network_detail.revoke_credential')" text size="small" severity="danger"
                                @click="revokeCredential(data)" />
                        </div>
                    </template></Column>
                </DataTable>
            </TabPanel>

            <!-- settings -->
            <TabPanel value="settings">
              <div v-if="!settingsLoaded" class="loading-rows"><Skeleton v-for="i in 4" :key="i" height="2rem" /></div>
              <div v-else class="settings-form">
                <p class="text-sm muted m-0">{{ t('web.mesh_network.temporary_settings_hint') }}</p>
                <div class="flex flex-col gap-1">
                    <div class="flex items-center">
                        <label for="settings-virtual-cidr">{{ t('web.network_list.virtual_cidr') }}</label>
                        <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('web.network_list.virtual_cidr_hint')"></span>
                    </div>
                    <InputText id="settings-virtual-cidr" v-model="settingsForm.virtual_cidr" class="font-mono"
                        @blur="settingsForm.virtual_cidr = normalizeVirtualSubnet(settingsForm.virtual_cidr)"
                        :placeholder="t('web.network_list.virtual_cidr_placeholder')" />
                </div>
                <div v-if="gatewayFollowed()" class="flex items-start gap-2 p-3 surface-100 rounded-md">
                    <i class="pi pi-check-circle text-green-500 mt-0.5"></i>
                    <div class="text-sm">{{ t('web.network_detail.gateway_followed') }}</div>
                </div>
                <div class="flex items-center">
                    <label for="settings-initial-nodes">{{ t('initial_nodes') }}</label>
                    <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('initial_nodes_help')"></span>
                </div>
                <div class="items-center flex flex-col p-fluid gap-y-2">
                    <UrlListInput id="settings-initial-nodes" v-model="initialNodes" :protos="protos"
                        defaultUrl="tcp://:11010" :add-label="t('add_initial_node')"
                        :placeholder="t('initial_node_placeholder')" />
                </div>
                <div class="flex gap-2">
                    <Button :label="t('web.common.save')" icon="pi pi-check" :loading="savingSettings" :disabled="settingsSecretMissing" @click="saveSettings" />
                </div>
            </div>

            </TabPanel>

            </TabPanels>
        </Tabs>

        <!-- add member dialog -->
        <Dialog v-model:visible="addMemberVisible" modal class="console-dialog" :header="t('web.network_detail.add_member')" :style="{ width: '32rem' }">
            <div class="flex flex-col gap-2">
                <div v-if="candidateDevices.length === 0" class="text-sm text-500">
                    {{ t('web.network_detail.no_candidate_devices') }}
                </div>
            </div>
            <DataTable v-model:selection="selectedDevices" :value="candidateDevices" dataKey="machine_id" size="small" scrollable
                scrollHeight="20rem" selectionMode="multiple">
                <Column selectionMode="multiple" style="width: 3rem" />
                <Column field="hostname" :header="t('web.device.hostname')" />
                <Column field="easytier_version" :header="t('web.device.version')" />
            </DataTable>
            <div class="flex flex-col gap-2 mt-2">
                <Message v-if="!network?.secure_mode" severity="warn" size="small" :closable="false">{{ t('web.network_detail.add_temporary_needs_secure') }}</Message>
                <p v-else class="text-sm muted m-0">{{ t('web.network_detail.add_temporary_hint') }}</p>
                <div class="flex items-start gap-2">
                    <label class="text-sm shrink-0 pt-1.5">{{ t('web.network_detail.credential_ttl') }}</label>
                    <CredentialTtlPicker v-model="addTtl" />
                </div>
            </div>
            <template #footer>
                <Button :label="t('web.common.cancel')" icon="pi pi-times" text severity="secondary" @click="addMemberVisible = false" />
                <Button :label="t('web.common.confirm')" icon="pi pi-check" :loading="adding" :disabled="selectedDevices.length === 0 || !network?.secure_mode"
                    @click="addMembers" />
            </template>
        </Dialog>

        <!-- edit member dialog -->
        <Dialog v-model:visible="editMemberVisible" modal class="console-dialog"
            :header="`${t('web.network_detail.edit_member')} · ${editingMember?.hostname ?? editingMember?.device_id ?? ''}`"
            :style="{ width: 'min(52rem, 96vw)' }" :maximizable="true">
            <Tabs :value="editTab" @update:value="(v: any) => editTab = v">
                <TabList>
                    <Tab value="basic">{{ t('web.network_detail.tab_basic_settings') }}</Tab>
                    <Tab value="advanced">{{ t('web.network_detail.advanced_config') }}</Tab>
                </TabList>
                <TabPanels>
                    <TabPanel value="basic">
                        <div class="flex flex-col gap-3 max-w-30rem">
                            <div class="flex flex-col gap-1">
                                <label for="edit-hostname-override">{{ t('web.network_detail.hostname_override') }}</label>
                                <InputText id="edit-hostname-override" v-model="editMemberForm.hostname_override"
                                    :placeholder="t('web.network_detail.hostname_override_placeholder')" />
                            </div>
                            <div class="flex flex-col gap-1">
                                <label for="edit-virtual-ipv4">{{ t('web.network_detail.static_ip') }}</label>
                                <InputText id="edit-virtual-ipv4" v-model="editMemberForm.virtual_ipv4" placeholder="10.126.126.10" />
                                <div class="text-xs text-500">{{ t('web.network_detail.static_ip_hint') }}</div>
                            </div>

                            <div class="flex flex-col gap-2">
                                <div class="flex items-center">
                                    <label>{{ t('proxy_cidrs') }}</label>
                                    <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('web.network_detail.proxy_cidr_hint')"></span>
                                </div>

                                <!-- 设备检测到的网段：点击即添加/移除 -->
                                <div v-if="candidateCidrs === undefined" class="text-xs muted">{{ t('web.console.loading') }}</div>
                                <div v-else-if="candidateCidrs.length > 0" class="flex flex-wrap gap-2">
                                    <Button v-for="cidr in candidateCidrs" :key="cidr"
                                        :label="cidr"
                                        :severity="editProxyCidrs.includes(cidr) ? 'primary' : 'secondary'"
                                        size="small" :outlined="!editProxyCidrs.includes(cidr)"
                                        class="font-mono"
                                        :aria-label="editProxyCidrs.includes(cidr)
                                            ? t('web.network_detail.proxy_remove_suggestion', { cidr })
                                            : t('web.network_detail.proxy_add_suggestion', { cidr })"
                                        @click="toggleCandidateCidr(cidr)" />
                                </div>
                                <div v-else class="text-xs muted">{{ t('web.network_detail.proxy_no_suggestions') }}</div>

                                <!-- 已配置（含手动添加的远端网段） -->
                                <div v-if="editProxyCidrs.length > 0" class="flex flex-wrap gap-2">
                                    <Tag v-for="cidr in editProxyCidrs" :key="cidr" :value="cidr"
                                        class="font-mono" closable @close="editProxyCidrs.splice(editProxyCidrs.indexOf(cidr), 1)" />
                                </div>

                                <!-- 手动输入兜底（远端网段/映射网段） -->
                                <div class="flex items-center gap-2">
                                    <InputText v-model="proxyCidrInput"
                                        :placeholder="t('web.network_detail.proxy_cidr_placeholder')"
                                        class="flex-1 font-mono"
                                        @keydown.enter.prevent="onProxyCidrEnter" />
                                    <Button icon="pi pi-plus" :label="t('web.common.add')" severity="secondary" outlined size="small"
                                        :aria-label="t('web.common.add')" @click="addProxyCidr" />
                                </div>
                            </div>
                        </div>
                    </TabPanel>
                    <TabPanel value="advanced">
                        <div v-if="!configLoaded" class="flex justify-center py-6"><ProgressSpinner /></div>
                        <Config v-else v-model:cur-network="configForm" :config-invalid="false" :hide-secure-mode="true"
                            :runtime-capabilities="configCapabilities"
                            :edit-vpn-portal-clients="true" />
                    </TabPanel>
                </TabPanels>
            </Tabs>
            <template #footer>
                <template v-if="editTab === 'basic'">
                    <Button :label="t('web.common.cancel')" icon="pi pi-times" text severity="secondary" @click="editMemberVisible = false" />
                    <Button :label="t('web.common.save')" icon="pi pi-check" :loading="savingMember" @click="saveMember" />
                </template>
                <template v-else>
                    <Button v-if="configMember?.has_override" :label="t('web.network_detail.reset_default')" icon="pi pi-replay"
                        severity="secondary" text :loading="configSaving" @click="resetMemberConfig" />
                    <Button :label="t('web.common.cancel')" icon="pi pi-times" text severity="secondary" @click="editMemberVisible = false" />
                    <Button :label="t('web.common.save')" icon="pi pi-check" :loading="configSaving" :disabled="!configLoaded" @click="saveMemberConfig" />
                </template>
            </template>
        </Dialog>

        <!-- generate credential dialog -->
        <Dialog v-model:visible="generateVisible" modal class="console-dialog"
            :header="t('web.network_detail.generate_credential')" :style="{ width: '34rem' }">
            <div v-if="!generatedCredential" class="flex flex-col gap-3">
                <div class="flex flex-col gap-1">
                    <label>{{ t('web.network_detail.credential_ttl') }}</label>
                    <CredentialTtlPicker v-model="generateForm.ttl" />
                </div>
                <div class="flex items-center gap-2">
                    <InputSwitch inputId="cred-reusable" v-model="generateForm.reusable" />
                    <label for="cred-reusable" class="text-sm">{{ t('web.network_detail.credential_reusable') }}</label>
                </div>
                <div class="text-xs text-500">{{ t('web.network_detail.credential_reusable_hint') }}</div>
            </div>
            <div v-else class="flex flex-col gap-3">
                <div class="text-sm">{{ t('web.network_detail.credential_secret_hint') }}</div>
                <div class="flex items-center gap-2">
                    <InputText :value="generatedCredential.credential_secret" readonly class="flex-1 font-mono text-xs" />
                    <Button icon="pi pi-copy" text rounded severity="secondary"
                        :aria-label="t('copy_config')" @click="copyText(generatedCredential.credential_secret, t('web.network_detail.generate_credential'))" />
                </div>
                <div class="text-sm font-semibold">{{ t('web.network_detail.join_command') }}</div>
                <div class="flex items-start gap-2">
                    <pre class="flex-1 font-mono text-xs whitespace-pre-wrap break-all surface-100 rounded-md p-3 m-0">{{ credentialJoinCommand(generatedCredential.credential_secret) }}</pre>
                    <Button icon="pi pi-copy" text rounded severity="secondary"
                        :aria-label="t('copy_config')" @click="copyText(credentialJoinCommand(generatedCredential.credential_secret), t('web.network_detail.join_command'))" />
                </div>
            </div>
            <template #footer>
                <Button v-if="!generatedCredential" :label="t('web.common.cancel')" icon="pi pi-times" text severity="secondary" @click="generateVisible = false" />
                <Button v-else :label="t('web.common.confirm')" icon="pi pi-check" text severity="secondary" @click="generateVisible = false" />
                <Button v-if="!generatedCredential" :label="t('web.network_detail.generate_credential')" icon="pi pi-key" :loading="generating" @click="generateCredential" />
            </template>
        </Dialog>

        <!-- join command viewer -->
        <Dialog :visible="joinCommandCredential !== null" modal class="console-dialog"
            :header="t('web.network_detail.join_command')" :style="{ width: '42rem' }"
            @update:visible="joinCommandCredential = null">
            <div v-if="joinCommandCredential" class="flex flex-col gap-3">
                <SelectButton v-model="joinFormat" :options="joinFormatOptions"
                    optionLabel="label" optionValue="value" />
                <div class="flex items-start gap-2">
                    <pre class="flex-1 font-mono text-xs whitespace-pre-wrap break-all surface-100 rounded-md p-3 m-0 select-text">{{ joinText(joinCommandCredential.credential_secret) }}</pre>
                    <Button icon="pi pi-copy" text rounded severity="secondary"
                        :aria-label="t('web.network_detail.credential_copy_command')"
                        v-tooltip.top="t('web.network_detail.credential_copy_command')"
                        @click="copyText(joinText(joinCommandCredential.credential_secret), t('web.network_detail.join_command'))" />
                </div>
            </div>
        </Dialog>

        <!-- node detail drawer -->
        <Drawer :visible="nodeDetailVisible" @update:visible="closeNodeDetail" position="right"
            class="console-node-drawer" :style="{ width: 'min(48rem, 100vw)' }">
            <template #header>
                <div class="node-drawer-heading">
                    <span class="node-drawer-icon pi pi-desktop" aria-hidden="true"></span>
                    <div class="node-drawer-identity">
                        <h2 class="node-drawer-title">{{ (nodeDetailMember?.hostname_override || nodeDetailMember?.alias || nodeDetailMember?.hostname) ?? nodeDetailMember?.device_id }}</h2>
                        <div class="node-drawer-meta">
                            <span v-if="nodeDetailMember?.runtime_virtual_ipv4 || nodeDetailMember?.virtual_ipv4 || nodeDetailMember?.allocated_ipv4" class="mono-value">{{ nodeDetailMember?.runtime_virtual_ipv4 ?? nodeDetailMember?.virtual_ipv4 ?? nodeDetailMember?.allocated_ipv4 }}</span>
                            <span v-if="nodeDetailMember?.version">v{{ nodeDetailMember.version.split('-')[0] }}</span>
                        </div>
                    </div>
                    <span class="node-drawer-presence">
                        <span class="status-dot" :class="nodeDetailMember?.online ? 'online' : 'offline'" aria-hidden="true"></span>
                        {{ t(nodeDetailMember?.online ? 'web.device.online' : 'web.device.offline') }}
                    </span>
                </div>
            </template>
            <div class="node-drawer-main">
            <Message v-if="nodeDetailError" severity="warn" :closable="false">{{ nodeDetailError }}</Message>
            <section class="node-drawer-summary" :aria-label="t('web.network_detail.node_overview')">
                <div class="node-drawer-metric node-drawer-metric-state">
                    <strong>{{ nodeDetailMember?.running === true ? t('web.network_detail.running') : nodeDetailMember?.running === false ? t('web.network_detail.stopped') : '—' }}</strong>
                    <span class="node-drawer-metric-label">{{ t('web.network_detail.instance_state') }}</span>
                </div>
                <div class="node-drawer-metric">
                    <strong>{{ nodePeers === undefined || nodeRoutes === undefined ? '—' : nodePeerRows.length }}</strong>
                    <span class="node-drawer-metric-label">{{ t('web.network_detail.node_peers') }}</span>
                </div>
                <div class="node-drawer-metric">
                    <strong>{{ nodeRoutes?.length ?? '—' }}</strong>
                    <span class="node-drawer-metric-label">{{ t('web.network_detail.node_routes') }}</span>
                </div>
                <div class="node-drawer-metric">
                    <strong>{{ nodePeers === undefined ? '—' : nodeConnRows.length }}</strong>
                    <span class="node-drawer-metric-label">{{ t('web.network_detail.node_conns') }}</span>
                </div>
            </section>
            <Tabs value="overview" class="node-drawer-tabs">
                <TabList>
                    <Tab value="overview">{{ t('web.network_detail.node_overview') }}</Tab>
                    <Tab value="general">{{ t('web.network_detail.node_actions') }}</Tab>
                </TabList>
                <TabPanels>
                    <TabPanel value="overview" class="node-drawer-panel">
                        <section v-if="!nodeDetailError || nodePeerRows.length > 0 || (nodePeers !== undefined && nodeRoutes !== undefined)" class="node-drawer-section">
                            <div v-if="(nodePeers === undefined && nodeRoutes === undefined) || nodePeerRows.length > 0" class="node-drawer-section-heading">
                                <h3>{{ t('web.network_detail.node_peers') }}</h3>
                            </div>
                            <div v-if="nodePeers === undefined && nodeRoutes === undefined" class="node-drawer-loading">
                                <Skeleton height="2.5rem" />
                                <Skeleton height="2.5rem" />
                            </div>
                            <div v-else-if="nodePeerRows.length === 0" class="node-drawer-empty">
                                <i class="pi pi-share-alt" aria-hidden="true"></i>
                                <span><strong>{{ t('web.network_detail.node_no_peers') }}</strong> · {{ t('web.network_detail.node_no_peers_hint') }}</span>
                            </div>
                            <template v-else>
                            <DataTable :value="nodePeerRows" dataKey="key" size="small" class="console-table node-drawer-desktop-peers" scrollable
                                :paginator="nodePeerRows.length > 15" :rows="15" tableStyle="min-width: 40rem">
                            <Column :header="t('device')">
                                <template #body="{ data }">
                                    <div class="flex flex-col gap-1">
                                        <div class="whitespace-nowrap">{{ data.ipv4 || '—' }}</div>
                                        <div>{{ data.hostname || `#${data.peer_id}` }}</div>
                                    </div>
                                </template>
                            </Column>
                            <Column :header="t('web.network_detail.route_cost')"><template #body="{ data }">
                                {{ routeCostLabel(data.cost) }}
                            </template></Column>
                            <Column :header="t('web.network_detail.conn_type')"><template #body="{ data }">
                                <span class="font-mono text-xs">{{ data.protocols.join(' / ') || '—' }}</span>
                            </template></Column>
                            <Column :header="t('web.network_detail.path_latency')"><template #body="{ data }">
                                {{ latencyLabel(data.latencyMs) }}
                            </template></Column>
                            <Column :header="t('total_traffic')"><template #body="{ data }">
                                <div class="whitespace-nowrap">{{ t('upload') }}: {{ data.traffic.upload }}</div>
                                <div class="whitespace-nowrap">{{ t('download') }}: {{ data.traffic.download }}</div>
                            </template></Column>
                            <Column :header="t('estimated_bandwidth')"><template #body="{ data }">
                                <div v-tooltip="t('estimated_bandwidth_help')" class="whitespace-nowrap">
                                    <div>{{ t('upload') }}: {{ data.bandwidth.upload }}</div>
                                    <div>{{ t('download') }}: {{ data.bandwidth.download }}</div>
                                </div>
                            </template></Column>
                            </DataTable>
                            <div class="node-drawer-mobile-peers">
                                <div v-for="peer in nodePeerRows" :key="peer.key" class="node-drawer-mobile-peer">
                                    <div class="node-drawer-mobile-peer-heading">
                                        <span>{{ peer.ipv4 || '—' }}</span>
                                        <strong>{{ peer.hostname || `#${peer.peer_id}` }}</strong>
                                    </div>
                                    <div class="node-drawer-mobile-peer-path">
                                        <span>{{ routeCostLabel(peer.cost) }}</span>
                                        <span>{{ peer.protocols.join(' / ') || '—' }}</span>
                                        <span>{{ latencyLabel(peer.latencyMs) }}</span>
                                    </div>
                                    <div class="mt-2">{{ t('total_traffic') }}: {{ t('upload') }} {{ peer.traffic.upload }} / {{ t('download') }} {{ peer.traffic.download }}</div>
                                    <div>{{ t('estimated_bandwidth') }}: {{ t('upload') }} {{ peer.bandwidth.upload }} / {{ t('download') }} {{ peer.bandwidth.download }}</div>
                                </div>
                            </div>
                            </template>
                        </section>
                        <details v-if="(nodeRoutes?.length ?? 0) > 0" class="node-drawer-disclosure">
                            <summary><span>{{ t('web.network_detail.node_routes') }}</span><span class="node-drawer-disclosure-count">{{ nodeRoutes?.length }}</span><i class="pi pi-chevron-down" aria-hidden="true"></i></summary>
                        <DataTable :value="nodeRoutes ?? []" size="small" class="console-table" scrollable
                            :paginator="(nodeRoutes?.length ?? 0) > 15" :rows="15" tableStyle="min-width: 40rem">
                            <Column :header="t('device')"><template #body="{ data }">
                                <div class="flex flex-col gap-1">
                                    <div class="whitespace-nowrap">{{ ipv4ToString(data.ipv4_addr) }}/{{ data.ipv4_addr?.network_length ?? 24 }}</div>
                                    <div>{{ data.hostname || `#${data.peer_id}` }}</div>
                                </div>
                            </template></Column>
                            <Column :header="t('web.network_detail.route_cost')"><template #body="{ data }">
                                {{ routeCostLabel(data.cost) }}
                            </template></Column>
                            <Column :header="t('web.network_detail.path_latency')"><template #body="{ data }">
                                {{ latencyLabel(data.path_latency) }}
                            </template></Column>
                            <Column field="proxy_cidrs" :header="t('proxy_cidrs')"><template #body="{ data }">
                                <span class="font-mono text-xs">{{ (data.proxy_cidrs ?? []).join(', ') || '—' }}</span>
                            </template></Column>
                            <Column field="version" :header="t('web.device.version')"><template #body="{ data }">
                                <span class="text-xs muted">v{{ (data.version || '?').split('-')[0] }}</span>
                            </template></Column>
                        </DataTable>
                        </details>
                        <details v-if="nodeConnRows.length > 0" class="node-drawer-disclosure">
                            <summary><span>{{ t('web.network_detail.node_conns') }}</span><span class="node-drawer-disclosure-count">{{ nodeConnRows.length }}</span><i class="pi pi-chevron-down" aria-hidden="true"></i></summary>
                        <DataTable :value="nodeConnRows" size="small" class="console-table" scrollable
                            :paginator="nodeConnRows.length > 15" :rows="15" tableStyle="min-width: 40rem">
                            <Column field="hostname" :header="t('web.device.hostname')" />
                            <Column :header="t('web.network_detail.conn_type')"><template #body="{ data }">
                                <span class="font-mono text-xs">{{ data.conn.tunnel?.tunnel_type || '—' }}</span>
                            </template></Column>
                            <Column :header="t('web.network_detail.conn_remote')"><template #body="{ data }">
                                <span class="font-mono text-xs">{{ data.conn.tunnel?.remote_addr?.url ?? '—' }}</span>
                            </template></Column>
                            <Column :header="t('web.network_detail.path_latency')"><template #body="{ data }">
                                {{ latencyLabel(connectionLatency(data.conn)) }}
                            </template></Column>
                            <Column :header="t('web.network_detail.loss_rate')"><template #body="{ data }">
                                {{ ((data.conn.loss_rate ?? 0) * 100).toFixed(1) + '%' }}
                            </template></Column>
                            <Column :header="t('web.network_detail.traffic')"><template #body="{ data }">
                                <div class="whitespace-nowrap">{{ t('upload') }}: {{ StatusDisplay.trafficBytes(data.conn.stats?.tx_bytes) }}</div>
                                <div class="whitespace-nowrap">{{ t('download') }}: {{ StatusDisplay.trafficBytes(data.conn.stats?.rx_bytes) }}</div>
                            </template></Column>
                            <Column :header="t('estimated_bandwidth')"><template #body="{ data }">
                                <div class="whitespace-nowrap">{{ t('upload') }}: {{ connectionBandwidth(data.conn).upload }}</div>
                                <div class="whitespace-nowrap">{{ t('download') }}: {{ connectionBandwidth(data.conn).download }}</div>
                            </template></Column>
                        </DataTable>
                        </details>
                        <details v-if="(nodeAclStats ?? []).length > 0" class="node-drawer-disclosure">
                            <summary><span>{{ t('web.network_detail.acl_stats') }}</span><span class="node-drawer-disclosure-count">{{ nodeAclStats?.length }}</span><i class="pi pi-chevron-down" aria-hidden="true"></i></summary>
                        <DataTable :value="nodeAclStats!" size="small" class="console-table" scrollable tableStyle="min-width: 32rem">
                            <Column :header="t('web.network_detail.acl_rule')"><template #body="{ data }">
                                {{ data.rule?.name || '—' }}
                            </template></Column>
                            <Column :header="t('web.network_detail.acl_packets')"><template #body="{ data }">
                                {{ data.stat?.packet_count ?? 0 }}
                            </template></Column>
                            <Column :header="t('web.network_detail.acl_bytes')"><template #body="{ data }">
                                {{ data.stat?.byte_count ?? 0 }}
                            </template></Column>
                        </DataTable>
                        </details>
                    </TabPanel>
                    <TabPanel value="general" class="node-drawer-panel node-drawer-actions">
                        <div class="node-drawer-action">
                            <div>
                                <div class="font-semibold">{{ t('web.network_detail.log_level') }}</div>
                                <div class="text-xs muted">{{ t('web.network_detail.log_level_hint') }}</div>
                            </div>
                            <Select :model-value="nodeLoggerLevel" :options="loggerLevelOptions" optionLabel="label"
                                optionValue="value" :loading="nodeLoggerLevel === undefined" :disabled="nodeLoggerSaving"
                                :placeholder="t('web.console.loading')"
                                class="min-w-40" @update:model-value="applyNodeLoggerLevel" />
                        </div>
                        <div class="node-drawer-action">
                            <div>
                                <div class="font-semibold">{{ t('web.network_detail.export_config') }}</div>
                                <div class="text-xs muted">{{ t('web.network_detail.export_config_hint') }}</div>
                            </div>
                            <Button :label="t('web.network_detail.export_config')" icon="pi pi-download" severity="secondary" outlined
                                @click="exportNodeConfig" />
                        </div>
                    </TabPanel>
                </TabPanels>
            </Tabs>
            </div>
        </Drawer>

        <!-- node config export dialog -->
        <Dialog v-model:visible="nodeConfigVisible" modal class="console-dialog"
            :header="`${t('web.network_detail.export_config')} · ${nodeDetailMember?.hostname ?? ''}`"
            :style="{ width: 'min(48rem, 96vw)' }">
            <ScrollPanel style="height: 60vh">
                <pre v-if="nodeTomlConfig" class="font-mono text-xs whitespace-pre-wrap">{{ nodeTomlConfig }}</pre>
                <div v-else-if="nodeTomlConfig === undefined" class="text-center py-6"><ProgressSpinner /></div>
                <div v-else class="text-center py-6 text-500">—</div>
            </ScrollPanel>
            <template #footer>
                <Button :label="t('copy_config')" icon="pi pi-copy" text severity="secondary" :disabled="!nodeTomlConfig" @click="copyNodeConfig" />
                <Button :label="t('web.network_detail.download')" icon="pi pi-download" text severity="secondary" :disabled="!nodeTomlConfig" @click="downloadNodeConfig" />
                <Button :label="t('web.common.cancel')" icon="pi pi-times" text severity="secondary" @click="nodeConfigVisible = false" />
            </template>
        </Dialog>
    </div>
</template>

<style scoped>
.member-card-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
    gap: 1rem;
}

.member-card {
    border: 1px solid var(--console-border);
    border-radius: 0.75rem;
    background: var(--surface-card);
    box-shadow: 0 1px 3px 0 rgba(0, 0, 0, 0.08);
    padding: 1rem 1.1rem;
    display: flex;
    flex-direction: column;
    transition: transform 0.2s ease, box-shadow 0.2s ease;
}

.mobile-list .member-card {
    border: 0;
    border-bottom: 1px solid var(--console-border);
    border-radius: 0;
    background: transparent;
    box-shadow: none;
    padding: 16px 0;
}

.mobile-list .member-card:hover {
    transform: none;
    box-shadow: none;
}

.mobile-list .member-card:last-child {
    border-bottom: 0;
}

/* 桌面卡片视图下恢复卡片的独立外观（移动端 .mobile-list 内仍是扁平列表项） */
.member-card-grid .member-card.member-card {
    border: 1px solid var(--console-border);
    border-radius: 0.75rem;
    background: var(--surface-card);
    box-shadow: 0 1px 3px 0 rgba(0, 0, 0, 0.08);
    padding: 1rem 1.1rem;
}

@media (max-width: 767px) {
    .member-card-grid {
        display: block;
    }

    .member-card-grid .member-card.member-card {
        border: 0;
        border-bottom: 1px solid var(--console-border);
        border-radius: 0;
        background: transparent;
        box-shadow: none;
        padding: 16px 0;
    }

    .member-card-grid .member-card.member-card:hover {
        transform: none;
        box-shadow: none;
    }

    .member-card-grid .member-card:last-child {
        border-bottom: 0;
    }
}

.member-card.offline {
    opacity: 0.72;
}

.member-title-row {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    min-width: 0;
}

.override-marker { color: var(--p-primary-color); font-size: 0.8rem; margin-right: 0.25rem; }

.member-actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.25rem;
    margin-top: 0.5rem;
    padding-top: 0.5rem;
    border-top: 1px solid var(--console-border);
}

:deep(tr.credential-row-highlight) {
    background-color: var(--p-highlight-background, rgba(59, 130, 246, 0.12));
    outline: 1px solid var(--p-primary-color, #3b82f6);
    outline-offset: -1px;
}
</style>
