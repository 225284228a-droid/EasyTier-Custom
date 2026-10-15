<script setup lang="ts">
import { v4 as uuidv4 } from 'uuid'
import { AutoComplete, Button, Checkbox, Dialog, Divider, InputNumber, InputSwitch, InputText, MultiSelect, Panel, Password, Select, SelectButton, ToggleButton } from 'primevue'
import VpnConfig from './VpnConfig.vue'
import InputGroup from 'primevue/inputgroup'
import InputGroupAddon from 'primevue/inputgroupaddon'
import {
  addRow,
  DEFAULT_NETWORK_CONFIG,
  defaultFlagValue,
  formFlags,
  NetworkConfig,
  normalizeNetworkConfig,
  removeRow,
} from '../types/network'
import { computed, ref, onMounted, onUnmounted, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import AclManager from './acl/AclManager.vue'
import UrlListInput from './UrlListInput.vue'
import { configFieldSupported, HTTP3_CAPABILITY } from '../modules/capabilities'
import { p2pDisguiseMode, setP2pDisguiseMode, type P2pDisguiseMode } from '../modules/p2pDisguise'
import { createVpnPortalConfig } from '../modules/vpnPortal'
import { isValidHostIp } from '../modules/ip'
import type { VpnPortalClientConfig } from '../types/network'
import type { VpnConfig as VpnConfigType } from '../generated/proto/api_manage'

const props = defineProps<{
  actionLabel?: string
  configInvalid?: boolean
  hostname?: string
  /// Hide the secure-mode switch and the temporary-credential entry; for
  /// callers where secure mode is a network-level property managed
  /// elsewhere.
  hideSecureMode?: boolean
  /// Central members save their clients through the configuration owner.
  editVpnPortalClients?: boolean
  runtimeCapabilities?: readonly string[]
}>()

defineEmits(['runNetwork'])

const curNetwork = defineModel('curNetwork', {
  type: Object as () => NetworkConfig,
  default: DEFAULT_NETWORK_CONFIG,
})

// --- network identity: shared secret (default) or admin-issued credential ---
const identityMode = ref<'secret' | 'credential'>('secret')

// A pasted credential switches the form to credential mode; replacing the
// whole config object (loading another network) re-derives the mode from it.
watch(
  () => curNetwork.value.credential_secret,
  (secret) => {
    if (secret) {
      identityMode.value = 'credential'
    }
  },
  { immediate: true },
)
watch(
  curNetwork,
  (network) => {
    identityMode.value = network.credential_secret ? 'credential' : 'secret'
  },
  { immediate: true },
)

const useCredentialMode = () => {
  identityMode.value = 'credential'
}

const useSecretMode = () => {
  // Switching back is an explicit choice: drop the credential so saving
  // uses the network secret instead of silently joining as the credential.
  curNetwork.value.credential_secret = undefined
  identityMode.value = 'secret'
}

const credentialInvalid = computed(
  () => identityMode.value === 'credential' && !(curNetwork.value.credential_secret ?? '').trim(),
)

const secureModeEnabled = computed({
  get: () => curNetwork.value.secure_mode?.enabled === true,
  set: (value: boolean) => {
    curNetwork.value.secure_mode = value ? { enabled: true } : undefined
  },
})

const { t } = useI18n()

const dhcpIpv4CidrInput = computed<string>({
  get: () => curNetwork.value.dhcp_ipv4_cidr ?? '',
  set: (value) => {
    curNetwork.value.dhcp_ipv4_cidr = value.trim() || undefined
  },
})

const dhcpv6Enabled = computed({
  get: () => curNetwork.value.dhcpv6
    ?? (curNetwork.value.dhcp_ipv6_cidr?.trim().length ?? 0) > 0,
  set: (enabled: boolean) => {
    curNetwork.value.dhcpv6 = enabled
  },
})

const dhcpIpv6CidrInput = computed<string>({
  get: () => curNetwork.value.dhcp_ipv6_cidr ?? '',
  set: (value) => {
    curNetwork.value.dhcp_ipv6_cidr = value.trim() || undefined
  },
})

const allProtos: { [proto: string]: number } = {
  tcp: 11010,
  udp: 11010,
  wg: 11011,
  ws: 11011,
  wss: 11012,
  quic: 11012,
  faketcp: 11013,
  http3: 11014,
  http: 80,
  https: 443,
  txt: 0,
  srv: 0,
}

const listenerExcludedProtos = new Set(['http', 'https', 'txt', 'srv'])
const protos = computed(() => Object.fromEntries(Object.entries(allProtos).filter(([proto]) =>
  proto !== 'http3' || props.runtimeCapabilities === undefined || props.runtimeCapabilities.includes(HTTP3_CAPABILITY))))
const listenerProtos = computed(() => Object.fromEntries(
  Object.entries(protos.value).filter(([proto]) => !listenerExcludedProtos.has(proto))))
const extensionSupported = (field: string) => props.runtimeCapabilities === undefined || configFieldSupported(field, props.runtimeCapabilities)
const disguiseSupported = computed(() => extensionSupported('p2p_disguise_mode'))

const inetSuggestions = ref([''])

function searchInetSuggestions(e: { query: string }) {
  if (e.query.search('/') >= 0) {
    inetSuggestions.value = [e.query]
  } else {
    const ret = []
    for (let i = 0; i < 32; i++) {
      ret.push(`${e.query}/${i}`)
    }
    inetSuggestions.value = ret
  }
}

const whitelistSuggestions = ref([''])

function searchWhitelistSuggestions(e: { query: string }) {
  const ret = []
  ret.push(e.query)
  whitelistSuggestions.value = ret
}

interface BoolFlag {
  field: keyof NetworkConfig
  help: string
}

const disguisedP2pFields = new Set<keyof NetworkConfig>([
  'only_use_wss_http3_for_hole_punching',
  'prefer_wss_http3_for_p2p',
  'disable_wss_http3_for_p2p',
])

const bool_flags: BoolFlag[] = [
  // The disguise flags share a dedicated selector; all other schema flags
  // use a checkbox, plus the one network option the form adds itself.
  ...formFlags().filter(flag => !disguisedP2pFields.has(flag.field)),
]
const visibleBoolFlags = computed(() => bool_flags.filter(flag => extensionSupported(flag.field)))

const disguisedP2pMode = computed<P2pDisguiseMode>({
  get: () => p2pDisguiseMode(curNetwork.value),
  set: value => setP2pDisguiseMode(curNetwork.value, value, props.runtimeCapabilities),
})

const disguisedP2pModeOptions = computed(() => [
  { label: t('p2p_disguise_mode_prefer'), value: 'prefer' },
  { label: t('p2p_disguise_mode_off'), value: 'off' },
  { label: t('p2p_disguise_mode_disabled'), value: 'disabled' },
  { label: t('p2p_disguise_mode_only'), value: 'only' },
])

type P2pPreferProtocol = 'tcp' | 'udp'

// The kernel's `default_protocol`: the transport automatic P2P tries first.
// It also orders the two disguised transports - "udp" prefers HTTP3 and
// "tcp" prefers WSS.
const p2pPreferProtocol = computed<P2pPreferProtocol>({
  get() {
    return (curNetwork.value.p2p_prefer_protocol ?? 'udp') === 'udp' ? 'udp' : 'tcp'
  },
  set(value: P2pPreferProtocol) {
    curNetwork.value.p2p_prefer_protocol = value
  },
})

const p2pPreferProtocolOptions = computed(() => [
  { label: t('p2p_prefer_protocol_tcp'), value: 'tcp' },
  { label: t('p2p_prefer_protocol_udp'), value: 'udp' },
])

/**
 * Whether a flag's checkbox is on: what the configuration states, or the value
 * the network runs with when it states nothing. A flag this config leaves unset
 * is not off, it is the default, and several default to on.
 */
function flagChecked(field: keyof NetworkConfig): boolean {
  const value = curNetwork.value[field]

  return typeof value === 'boolean' ? value : defaultFlagValue(field, props.runtimeCapabilities)
}

/** A checkbox states a flag; leaving it alone leaves the config silent. */
function setFlag(field: keyof NetworkConfig, value: boolean) {
  Object.assign(curNetwork.value, { [field]: value })
}

function ensureVpnConfig(): VpnConfigType {
  if (!curNetwork.value.vpn) {
    curNetwork.value.vpn = {
      exit_enabled: true,
      rules: [],
    }
  }
  const config = curNetwork.value.vpn
  config.rules ??= []
  return config
}

const vpnExitEnabled = computed({
  get: () => curNetwork.value.vpn?.exit_enabled ?? true,
  set: (enabled: boolean) => {
    ensureVpnConfig().exit_enabled = enabled
  },
})

const portForwardProtocolOptions = ref(["tcp", "udp"]);

const editingPortForward = ref(false);
const editingPortForwardIndex = ref(-1);
const editingPortForwardData = ref();

function openPortForwardEditor(index: number) {
  editingPortForwardIndex.value = index;
  // deep copy
  editingPortForwardData.value = JSON.parse(JSON.stringify(curNetwork.value.port_forwards[index]));
  editingPortForward.value = true;
}

function addPortForward() {
  addRow(curNetwork.value.port_forwards)
  if (isCompact.value) {
    openPortForwardEditor(curNetwork.value.port_forwards.length - 1)
  }
}

function savePortForward() {
  if (portForwardEditorInvalid.value) return;
  curNetwork.value.port_forwards[editingPortForwardIndex.value] = editingPortForwardData.value;
  editingPortForward.value = false;
}

/** An empty address is a row not filled in yet; a typed one must parse. */
function portForwardIpInvalid(ip: string | null | undefined): boolean {
  return !!ip && !isValidHostIp(ip);
}

const portForwardEditorInvalid = computed(() => {
  const data = editingPortForwardData.value;
  return !data || !isValidHostIp(data.bind_ip) || !isValidHostIp(data.dst_ip);
});

const portForwardContainer = ref<HTMLElement | null>(null);
const isCompact = ref(false);

const UINT64_MAX = (1n << 64n) - 1n

onMounted(() => {
  if (portForwardContainer.value) {
    let resizeObserver = new ResizeObserver(entries => {
      for (const entry of entries) {
        isCompact.value = entry.contentRect.width < 540;
      }
    });
    resizeObserver.observe(portForwardContainer.value);

    onUnmounted(() => {
      if (resizeObserver && portForwardContainer.value) {
        resizeObserver.unobserve(portForwardContainer.value);
      }
    });
  }
});

function syncNormalizedNetwork(network: NetworkConfig | undefined): void {
  if (!network) {
    return
  }

  Object.assign(network, normalizeNetworkConfig(network))
  if (network.vpn_portal_config?.enabled !== false && network.vpn_portal_config
    && !network.vpn_portal_config.wireguard_private_key) {
    network.vpn_portal_config.wireguard_private_key = createVpnPortalConfig().wireguard_private_key
  }
}

watch(() => curNetwork.value, syncNormalizedNetwork, { immediate: true, deep: false })

function parseInstanceRecvBpsLimitInput(value: string): number | string | null | undefined {
  const trimmed = value.trim()
  if (trimmed.length === 0) {
    return null
  }
  if (!/^\d+$/.test(trimmed)) {
    return undefined
  }

  const limit = BigInt(trimmed)
  if (limit > UINT64_MAX) {
    return undefined
  }

  return limit <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(limit) : limit.toString()
}

const instanceRecvBpsLimitInput = computed<string>({
  get: () => {
    const limit = curNetwork.value.instance_recv_bps_limit
    return limit == null ? '' : String(limit)
  },
  set: (value) => {
    const limit = parseInstanceRecvBpsLimitInput(value)
    if (limit !== undefined) {
      curNetwork.value.instance_recv_bps_limit = limit
    }
  },
})

const vpnPortalEnabled = computed({
  get: () => curNetwork.value.vpn_portal_config !== undefined
    && curNetwork.value.vpn_portal_config.enabled !== false,
  set: (enabled: boolean) => {
    if (enabled && !curNetwork.value.vpn_portal_config) {
      curNetwork.value.vpn_portal_config = createVpnPortalConfig()
    } else if (curNetwork.value.vpn_portal_config) {
      if (enabled && !curNetwork.value.vpn_portal_config.wireguard_private_key) {
        curNetwork.value.vpn_portal_config.wireguard_private_key = createVpnPortalConfig().wireguard_private_key
      }
      curNetwork.value.vpn_portal_config.enabled = enabled
    }
  },
})

const vpnPortalGroupOptions = computed(() => (
  curNetwork.value.acl?.acl_v1?.group?.declares ?? []
).map((group) => group.group_name))

const vpnPortalClientViewKeys = new WeakMap<VpnPortalClientConfig, string>()

function vpnPortalClientViewKey(client: VpnPortalClientConfig): string {
  let key = vpnPortalClientViewKeys.get(client)
  if (!key) {
    key = uuidv4()
    vpnPortalClientViewKeys.set(client, key)
  }
  return key
}

function addVpnPortalClient() {
  curNetwork.value.vpn_portal_config?.clients.push({
    name: `device-${uuidv4().slice(0, 8)}`, virtual_ip: '', groups: [],
  })
}

function removeVpnPortalClient(index: number) {
  curNetwork.value.vpn_portal_config?.clients.splice(index, 1)
}
</script>

<template>
  <div class="frontend-lib">
    <div class="flex flex-col h-full">
      <div class="flex flex-col">
        <div class="w-full self-center ">
          <Panel :header="t('basic_settings')">
            <div class="flex flex-col gap-y-2">
              <div class="flex flex-row gap-x-9 flex-wrap">
                <div class="flex flex-col gap-2 basis-5/12 grow">
                  <div class="flex items-center" for="virtual_ip">
                    <label class="mr-2"> {{ t('virtual_ipv4') }} </label>
                    <Checkbox v-model="curNetwork.dhcp" input-id="virtual_ip_auto" :binary="true" />

                    <label for="virtual_ip_auto" class="ml-2">
                      {{ t('virtual_ipv4_dhcp') }}
                    </label>
                  </div>
                  <InputGroup>
                    <InputText id="virtual_ip" v-model="curNetwork.virtual_ipv4" :disabled="curNetwork.dhcp"
                      aria-describedby="virtual_ipv4-help" />
                    <InputGroupAddon>
                      <span>/</span>
                    </InputGroupAddon>
                    <InputNumber v-model="curNetwork.network_length" :disabled="curNetwork.dhcp"
                      inputId="horizontal-buttons" showButtons :step="1" mode="decimal" :min="1" :max="32" fluid
                      class="max-w-20" />
                  </InputGroup>
                  <div v-if="curNetwork.dhcp" class="flex flex-col gap-2">
                    <label for="dhcp_ipv4_cidr">{{ t('dhcp_ipv4_cidr') }}</label>
                    <InputText id="dhcp_ipv4_cidr" v-model="dhcpIpv4CidrInput"
                      :placeholder="t('dhcp_ipv4_cidr_placeholder')" />
                    <small class="text-xs">{{ t('dhcp_ipv4_cidr_help') }}</small>
                  </div>
                </div>
                <div class="flex flex-col gap-2 basis-5/12 grow">
                  <div class="flex items-center">
                    <Checkbox v-model="dhcpv6Enabled" input-id="dhcpv6" :binary="true" />
                    <label for="dhcpv6" class="ml-2">{{ t('dhcpv6') }}</label>
                  </div>
                  <div v-if="dhcpv6Enabled" class="flex flex-col gap-2">
                    <label for="dhcp_ipv6_cidr">{{ t('dhcp_ipv6_cidr') }}</label>
                    <InputText id="dhcp_ipv6_cidr" v-model="dhcpIpv6CidrInput"
                      :placeholder="t('dhcp_ipv6_cidr_placeholder')" />
                  </div>
                </div>
              </div>

              <div class="flex flex-row gap-x-9 flex-wrap">
                <div class="flex flex-col gap-2 basis-5/12 grow">
                  <label for="network_name">{{ t('network_name') }}</label>
                  <InputText id="network_name" v-model="curNetwork.network_name" aria-describedby="network_name-help" />
                  <div v-if="!hideSecureMode && identityMode === 'secret'" class="flex items-center gap-2">
                    <InputSwitch inputId="secure_mode" v-model="secureModeEnabled" />
                    <label for="secure_mode" class="cursor-pointer">{{ t('secure_mode') }}</label>
                    <span class="pi pi-question-circle text-sm" v-tooltip.top="t('secure_mode_hint')"></span>
                  </div>
                </div>
                <div v-if="identityMode === 'secret'" class="flex flex-col gap-2 basis-5/12 grow">
                  <label for="network_secret">{{ t('network_secret') }}</label>
                  <Password id="network_secret" v-model="curNetwork.network_secret"
                    aria-describedby="network_secret-help" toggleMask :feedback="false" fluid />
                  <template v-if="!hideSecureMode">
                    <button type="button" class="self-start text-sm underline cursor-pointer bg-transparent border-none p-0"
                      @click="useCredentialMode">
                      {{ t('use_credential') }} <i class="pi pi-angle-right"></i>
                    </button>
                  </template>
                </div>
                <div v-else class="flex flex-col gap-2 basis-5/12 grow">
                  <label for="credential_secret">{{ t('credential_secret') }}</label>
                  <Password id="credential_secret" v-model="curNetwork.credential_secret"
                    aria-describedby="credential_secret-help" toggleMask :feedback="false" fluid
                    :class="{ 'p-invalid': credentialInvalid }" />
                  <small id="credential_secret-help" class="text-xs">{{ t('credential_secret_hint') }}</small>
                  <template v-if="!hideSecureMode">
                    <div class="text-xs">{{ t('credential_mode_hint') }}</div>
                    <button type="button" class="self-start text-sm underline cursor-pointer bg-transparent border-none p-0"
                      @click="useSecretMode">
                      <i class="pi pi-angle-left"></i> {{ t('use_network_secret') }}
                    </button>
                  </template>
                </div>
              </div>

              <div class="flex flex-row gap-x-9 flex-wrap">
                <div class="flex flex-col gap-2 basis-5/12 grow">
                  <div class="flex items-center">
                    <label for="initial_nodes">{{ t('initial_nodes') }}</label>
                    <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('initial_nodes_help')"></span>
                  </div>
                  <div class="items-center flex flex-col p-fluid gap-y-2">
                    <UrlListInput id="initial_nodes" v-model="curNetwork.peer_urls" :protos="protos"
                      defaultUrl="tcp://:11010" :add-label="t('add_initial_node')"
                      :placeholder="t('initial_node_placeholder')" />
                  </div>
                </div>
              </div>
            </div>
          </Panel>

          <Divider />

          <Panel :header="t('advanced_settings')" toggleable collapsed>
            <div class="flex flex-col gap-y-2">

              <div class="flex flex-row gap-x-9 flex-wrap">
                <div class="flex flex-col gap-2 basis-5/12 grow">
                  <label> {{ t('flags_switch') }} </label>
                  <div class="flex flex-row flex-wrap">

                    <div class="basis-[20rem] flex items-center" v-for="flag in visibleBoolFlags" :key="flag.field">
                      <Checkbox :model-value="flagChecked(flag.field)" :input-id="flag.field" :binary="true"
                        @update:model-value="setFlag(flag.field, $event)" />
                      <label :for="flag.field" class="ml-2"> {{ t(flag.field) }} </label>
                      <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t(flag.help)"></span>
                    </div>

                    <div v-if="extensionSupported('vpn')" class="basis-[20rem] flex min-w-0 items-center">
                      <Checkbox v-model="vpnExitEnabled" input-id="vpn_exit_enabled" :binary="true" />
                      <label for="vpn_exit_enabled" class="ml-2">{{ t('vpn_exit_enabled') }}</label>
                      <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('vpn_exit_enabled_help')"></span>
                    </div>

                  </div>
                </div>
              </div>

              <div class="flex flex-row gap-x-9 flex-wrap">
                <div class="flex flex-col gap-2 basis-5/12 grow">
                  <label for="hostname">{{ t('hostname') }}</label>
                  <InputText id="hostname" v-model="curNetwork.hostname" aria-describedby="hostname-help" :format="true"
                    :placeholder="t('hostname_placeholder', [props.hostname])" />
                </div>
              </div>

              <div v-if="extensionSupported('p2p_prefer_protocol') || disguiseSupported" class="flex flex-row gap-x-9 flex-wrap">
                <div v-if="extensionSupported('p2p_prefer_protocol')" class="flex flex-col gap-2 basis-5/12 grow">
                  <div class="flex items-center">
                    <label for="p2p_prefer_protocol">{{ t('p2p_prefer_protocol') }}</label>
                    <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('p2p_prefer_protocol_help')"></span>
                  </div>
                  <SelectButton id="p2p_prefer_protocol" v-model="p2pPreferProtocol"
                    :options="p2pPreferProtocolOptions" option-label="label" option-value="value" :allow-empty="false" fluid />
                </div>

                <div v-if="disguiseSupported" class="flex flex-col gap-2 basis-5/12 grow">
                  <div class="flex items-center">
                    <label for="p2p_disguise_mode">{{ t('p2p_disguise_mode') }}</label>
                    <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('p2p_disguise_mode_help')"></span>
                  </div>
                  <Select id="p2p_disguise_mode" v-model="disguisedP2pMode"
                    :options="disguisedP2pModeOptions" option-label="label" option-value="value"
                    overlay-class="max-w-[calc(100vw-2rem)]" :pt="{ optionLabel: { class: 'whitespace-normal break-words' } }" fluid />
                </div>
              </div>

              <div v-if="extensionSupported('sni')" class="flex flex-row gap-x-9 flex-wrap">
                <div class="flex flex-col gap-2 basis-5/12 grow">
                  <label for="sni">{{ t('sni') }}</label>
                  <InputText id="sni" v-model="curNetwork.sni" aria-describedby="sni-help" :format="true"
                    :placeholder="t('sni_placeholder')" />
                </div>
              </div>

              <div class="flex flex-row gap-x-9 flex-wrap w-full">
                <div class="flex flex-col gap-2 grow p-fluid">
                  <label for="username">{{ t('proxy_cidrs') }}</label>
                  <AutoComplete id="subnet-proxy" v-model="curNetwork.proxy_cidrs"
                    :placeholder="t('chips_placeholder', ['10.0.0.0/24'])" class="w-full" multiple fluid
                    :suggestions="inetSuggestions" @complete="searchInetSuggestions" />
                </div>
              </div>

              <div class="flex flex-col gap-3">
                <div class="flex items-center gap-3">
                  <Checkbox v-model="vpnPortalEnabled" input-id="vpn_portal_enabled" :binary="true" />
                  <label for="vpn_portal_enabled">{{ t('vpn_portal_enable') }}</label>
                </div>
                <p class="text-sm text-surface-500 dark:text-surface-400">{{ t(editVpnPortalClients ? 'vpn_portal_config_clients_help' : 'vpn_portal_setup_help') }}</p>
                <details v-if="vpnPortalEnabled && curNetwork.vpn_portal_config">
                  <summary class="cursor-pointer text-sm">{{ t('vpn_portal_advanced') }}</summary>
                  <div class="flex flex-col gap-2 mt-3">
                    <label for="vpn_portal_wireguard_listen">{{ t('vpn_portal_wireguard_listen') }}</label>
                    <InputText id="vpn_portal_wireguard_listen" v-model="curNetwork.vpn_portal_config.wireguard_listen"
                      :placeholder="t('vpn_portal_wireguard_listen_placeholder')" />
                  </div>
                </details>
                <template v-if="editVpnPortalClients && vpnPortalEnabled && curNetwork.vpn_portal_config">
                  <div class="flex items-center justify-between gap-3">
                    <label>{{ t('vpn_portal_clients') }}</label>
                    <Button icon="pi pi-plus" :label="t('vpn_portal_add_client')" severity="secondary" size="small"
                      :disabled="curNetwork.vpn_portal_config.clients.length >= 64" @click="addVpnPortalClient" />
                  </div>
                  <div v-if="curNetwork.vpn_portal_config.clients.length === 0"
                    class="text-sm text-surface-500 dark:text-surface-400">{{ t('vpn_portal_no_clients') }}</div>
                  <div v-for="(client, index) in curNetwork.vpn_portal_config.clients" :key="vpnPortalClientViewKey(client)"
                    class="flex flex-row gap-3 flex-wrap items-end rounded border border-surface-200 dark:border-surface-700 p-3">
                    <div class="flex flex-col gap-2 grow basis-3/12">
                      <label :for="`vpn_portal_client_name_${index}`">{{ t('vpn_portal_client_name') }}</label>
                      <InputText :id="`vpn_portal_client_name_${index}`" v-model="client.name" />
                    </div>
                    <div class="flex flex-col gap-2 grow basis-3/12">
                      <label :for="`vpn_portal_client_virtual_ip_${index}`">{{ t('vpn_portal_client_virtual_ip') }}</label>
                      <InputText :id="`vpn_portal_client_virtual_ip_${index}`" v-model="client.virtual_ip"
                        :placeholder="t('vpn_portal_client_virtual_ip_placeholder')" />
                    </div>
                    <div v-if="vpnPortalGroupOptions.length" class="flex flex-col gap-2 grow basis-4/12">
                      <label :for="`vpn_portal_client_groups_${index}`">{{ t('vpn_portal_client_groups') }}</label>
                      <MultiSelect :input-id="`vpn_portal_client_groups_${index}`" v-model="client.groups"
                        :options="vpnPortalGroupOptions" appendTo="self" filter fluid
                        :placeholder="t('vpn_portal_client_groups_placeholder')" />
                    </div>
                    <Button icon="pi pi-trash" severity="danger" text rounded
                      :aria-label="t('vpn_portal_remove_client')" @click="removeVpnPortalClient(index)" />
                  </div>
                </template>
              </div>

              <div class="flex flex-row gap-x-9 flex-wrap">
                <div class="flex flex-col gap-2 grow p-fluid">
                  <label for="listener_urls">{{ t('listener_urls') }}</label>
                  <UrlListInput v-model="curNetwork.listener_urls" :protos="listenerProtos"
                    :add-label="t('add_listener_url')" placeholder="0.0.0.0" />
                </div>
              </div>

              <div class="flex flex-row gap-x-9 flex-wrap">
                <div class="flex flex-col gap-2 basis-5/12 grow">
                  <label for="dev_name">{{ t('dev_name') }}</label>
                  <InputText id="dev_name" v-model="curNetwork.dev_name" aria-describedby="dev_name-help" :format="true"
                    :placeholder="t('dev_name_placeholder')" />
                </div>
              </div>

              <div class="flex flex-row gap-x-9 flex-wrap">
                <div class="flex flex-col gap-2 basis-5/12 grow">
                  <div class="flex">
                    <label for="mtu">{{ t('mtu') }}</label>
                    <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('mtu_help')"></span>
                  </div>
                  <InputNumber id="mtu" v-model="curNetwork.mtu" aria-describedby="mtu-help" :format="false"
                    :placeholder="t('mtu_placeholder')" :min="400" :max="1380" fluid />
                </div>
              </div>

              <div class="flex flex-row gap-x-9 flex-wrap">
                <div class="flex flex-col gap-2 basis-5/12 grow">
                  <div class="flex">
                    <label for="instance_recv_bps_limit">{{ t('instance_recv_bps_limit') }}</label>
                    <span class="pi pi-question-circle ml-2 self-center"
                      v-tooltip="t('instance_recv_bps_limit_help')"></span>
                  </div>
                  <InputText id="instance_recv_bps_limit" v-model="instanceRecvBpsLimitInput"
                    aria-describedby="instance_recv_bps_limit-help" inputmode="numeric" pattern="[0-9]*"
                    :placeholder="t('instance_recv_bps_limit_placeholder')" fluid />
                </div>
              </div>

              <div class="flex flex-row gap-x-9 flex-wrap">
                <div class="flex flex-col gap-2 basis-5/12 grow">
                  <div class="flex">
                    <label for="relay_network_whitelist">{{ t('relay_network_whitelist') }}</label>
                    <span class="pi pi-question-circle ml-2 self-center"
                      v-tooltip="t('relay_network_whitelist_help')"></span>
                  </div>
                  <ToggleButton v-model="curNetwork.enable_relay_network_whitelist" on-icon="pi pi-check"
                    off-icon="pi pi-times" :on-label="t('off_text')" :off-label="t('on_text')" class="w-48" />
                  <div v-if="curNetwork.enable_relay_network_whitelist" class="items-center flex flex-row gap-x-4">
                    <div class="min-w-64 w-full">
                      <AutoComplete id="relay_network_whitelist" v-model="curNetwork.relay_network_whitelist"
                        :placeholder="t('relay_network_whitelist')" class="w-full" multiple fluid
                        :suggestions="whitelistSuggestions" @complete="searchWhitelistSuggestions" />
                    </div>
                  </div>
                </div>
              </div>

              <div class="flex flex-row gap-x-9 flex-wrap ">
                <div class="flex flex-col gap-2 grow">
                  <div class="flex">
                    <label for="routes">{{ t('manual_routes') }}</label>
                    <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('manual_routes_help')"></span>
                  </div>
                  <ToggleButton v-model="curNetwork.enable_manual_routes" on-icon="pi pi-check" off-icon="pi pi-times"
                    :on-label="t('off_text')" :off-label="t('on_text')" class="w-48" />
                  <div v-if="curNetwork.enable_manual_routes" class="items-center flex flex-row gap-x-4">
                    <div class="min-w-64 w-full">
                      <AutoComplete id="routes" v-model="curNetwork.routes"
                        :placeholder="t('chips_placeholder', ['192.168.0.0/16'])" class="w-full" multiple fluid
                        :suggestions="inetSuggestions" @complete="searchInetSuggestions" />
                    </div>
                  </div>
                </div>
              </div>

              <div class="flex flex-row gap-x-9 flex-wrap ">
                <div class="flex flex-col gap-2 grow">
                  <div class="flex">
                    <label for="socks5_port">{{ t('socks5') }}</label>
                    <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('socks5_help')"></span>
                  </div>
                  <ToggleButton v-model="curNetwork.enable_socks5" on-icon="pi pi-check" off-icon="pi pi-times"
                    :on-label="t('off_text')" :off-label="t('on_text')" class="w-48" />
                  <div v-if="curNetwork.enable_socks5" class="items-center flex flex-row gap-x-4">
                    <div class="min-w-64 w-full">
                      <InputNumber id="socks5_port" v-model="curNetwork.socks5_port" aria-describedby="rpc_port-help"
                        :format="false" :allow-empty="false" :min="0" :max="65535" class="w-full" />
                    </div>
                  </div>
                </div>
              </div>

              <div v-if="extensionSupported('vpn')" class="flex flex-row gap-x-9 flex-wrap w-full">
                <div class="flex flex-col gap-2 grow p-fluid">
                  <div class="flex">
                    <label>{{ t('vpn_section') }}</label>
                    <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('vpn_section_help')"></span>
                  </div>
                  <VpnConfig :network="curNetwork" />
                </div>
              </div>

              <div class="flex flex-row gap-x-9 flex-wrap w-full">
                <div class="flex flex-col gap-2 grow p-fluid">
                  <div class="flex">
                    <label for="mapped_listeners">{{ t('mapped_listeners') }}</label>
                    <span class="pi pi-question-circle ml-2 self-center" v-tooltip="t('mapped_listeners_help')"></span>
                  </div>
                  <UrlListInput v-model="curNetwork.mapped_listeners" :protos="protos"
                    :add-label="t('add_mapped_listener')" />
                </div>
              </div>

            </div>
          </Panel>

          <Divider />

          <Panel :header="t('port_forwards')" toggleable collapsed>
            <div ref="portForwardContainer" class="flex flex-col gap-y-2">
              <div class="flex flex-row gap-x-9 flex-wrap w-full">
                <div class="flex flex-col gap-2 grow p-fluid">
                  <div class="flex">
                    <label for="port_forwards">{{ t('port_forwards_help') }}</label>
                  </div>
                  <div v-for="(row, index) in curNetwork.port_forwards" :key="index" class="form-row">
                    <!-- Wide screen view -->
                    <div v-if="!isCompact" class="flex gap-2 items-end">
                      <SelectButton id="port_forward_proto" v-model="row.proto"
                        :options="portForwardProtocolOptions" :allow-empty="false" />
                      <div style="flex-grow: 4;">
                        <InputGroup>
                          <InputText v-model="row.bind_ip" :placeholder="t('port_forwards_bind_addr')"
                            :class="{ 'p-invalid': portForwardIpInvalid(row.bind_ip) }" />
                          <InputGroupAddon>
                            <span style="font-weight: bold">:</span>
                          </InputGroupAddon>
                          <InputNumber v-model="row.bind_port" :format="false" inputId="horizontal-buttons" :step="1"
                            mode="decimal" :min="1" :max="65535" fluid class="max-w-20" />
                        </InputGroup>
                      </div>
                      <div style="flex-grow: 4;">
                        <InputGroup>
                          <InputText v-model="row.dst_ip" :placeholder="t('port_forwards_dst_addr')"
                            :class="{ 'p-invalid': portForwardIpInvalid(row.dst_ip) }" />
                          <InputGroupAddon>
                            <span style="font-weight: bold">:</span>
                          </InputGroupAddon>
                          <InputNumber v-model="row.dst_port" :format="false" inputId="horizontal-buttons" :step="1"
                            mode="decimal" :min="1" :max="65535" fluid class="max-w-20" />
                        </InputGroup>
                      </div>
                      <div style="flex-grow: 1;">
                        <Button v-if="curNetwork.port_forwards.length > 0" icon="pi pi-trash" severity="danger" text
                          rounded @click="removeRow(index, curNetwork.port_forwards)" />
                      </div>
                    </div>
                    <!-- Small screen view -->
                    <div v-else class="flex justify-between items-center p-2 border-b">
                      <span>{{ row.proto }}://{{ row.bind_ip }}:{{ row.bind_port }}/{{ row.dst_ip }}:{{
                        row.dst_port }}</span>
                      <div class="flex gap-2">
                        <Button icon="pi pi-pencil" class="p-button-sm" @click="openPortForwardEditor(index)" />
                        <Button icon="pi pi-trash" class="p-button-sm p-button-danger"
                          @click="removeRow(index, curNetwork.port_forwards)" />
                      </div>
                    </div>
                  </div>

                  <div class="flex justify-content-end mt-4">
                    <Button icon="pi pi-plus" :label="t('port_forwards_add_btn')" severity="success"
                      @click="addPortForward" />
                  </div>

                  <Dialog v-model:visible="editingPortForward" modal :header="t('edit_port_forward')"
                    :style="{ width: '90vw', maxWidth: '600px' }">
                    <div v-if="editingPortForwardData" class="flex flex-col gap-4">
                      <SelectButton id="port_forward_proto_edit" v-model="editingPortForwardData.proto"
                        :options="portForwardProtocolOptions" :allow-empty="false" />
                      <InputGroup>
                        <InputText v-model="editingPortForwardData.bind_ip"
                          :placeholder="t('port_forwards_bind_addr')"
                          :class="{ 'p-invalid': portForwardIpInvalid(editingPortForwardData.bind_ip) }" />
                        <InputGroupAddon>
                          <span style="font-weight: bold">:</span>
                        </InputGroupAddon>
                        <InputNumber v-model="editingPortForwardData.bind_port" :format="false" :step="1" mode="decimal"
                          :min="1" :max="65535" class="max-w-20" />
                      </InputGroup>
                      <InputGroup>
                        <InputText v-model="editingPortForwardData.dst_ip" :placeholder="t('port_forwards_dst_addr')"
                          :class="{ 'p-invalid': portForwardIpInvalid(editingPortForwardData.dst_ip) }" />
                        <InputGroupAddon>
                          <span style="font-weight: bold">:</span>
                        </InputGroupAddon>
                        <InputNumber v-model="editingPortForwardData.dst_port" :format="false" :step="1" mode="decimal"
                          :min="1" :max="65535" class="max-w-20" />
                      </InputGroup>
                      <small v-if="portForwardEditorInvalid" class="p-error">{{ t('port_forwards_invalid_ip') }}</small>
                    </div>
                    <template #footer>
                      <Button :label="t('web.common.cancel')" icon="pi pi-times" @click="editingPortForward = false"
                        text />
                      <Button :label="t('web.common.save')" icon="pi pi-save" :disabled="portForwardEditorInvalid"
                        @click="savePortForward" />
                    </template>
                  </Dialog>
                </div>
              </div>
            </div>
          </Panel>

          <Divider />

          <Panel :header="t('acl.title')" toggleable collapsed>
            <div v-if="curNetwork.acl" class="flex flex-col gap-y-2">
              <AclManager v-model="curNetwork.acl" />
            </div>
            <div v-else class="flex justify-center p-4">
              <Button :label="t('acl.enabled')"
                @click="curNetwork.acl = { acl_v1: { chains: [], group: { declares: [], members: [] } } }" />
            </div>
          </Panel>

          <div class="flex pt-6 justify-center">
            <Button :label="actionLabel || t('run_network')" icon="pi pi-arrow-right" icon-pos="right" :disabled="configInvalid || credentialInvalid"
              @click="$emit('runNetwork', curNetwork)" />
          </div>
        </div>
      </div>
    </div>
  </div>
</template>
