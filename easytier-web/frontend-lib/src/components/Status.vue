<script setup lang="ts">
import { useTimeAgo } from '@vueuse/core'
import { NetworkInstance, type NetworkConfig, type TunnelInfo, type NodeInfo, type PeerRoutePair, type GetVpnStatusResponse } from '../types/network'
import type { RemoteClient } from '../modules/api'
import { useI18n } from 'vue-i18n';
import { computed, onMounted, onUnmounted, ref, watch } from 'vue';
import { ipv4InetToString, ipv4ToString, ipv6ToString } from '../modules/utils';
import { estimatedBandwidth, latencyMs, lossRate, numericValue, peerConns, udpNatTypeName } from '../modules/statusDisplay';
import { Badge, DataTable, Column, Tag, Chip, Button, Dialog, Timeline, Divider, Card, } from 'primevue';
import NetworkChart from './NetworkChart.vue';
import VpnPortalDialog from './VpnPortalDialog.vue';

const props = defineProps<{
  curNetworkInst: NetworkInstance | null,
  api: RemoteClient,
  readonly?: boolean,
}>()

const { t } = useI18n()

const peerRouteInfos = computed(() => {
  if (props.curNetworkInst) {
    const my_node_info = props.curNetworkInst.detail?.my_node_info
    return [{
      route: {
        ipv4_addr: my_node_info?.virtual_ipv4,
        hostname: my_node_info?.hostname,
        version: my_node_info?.version,
        stun_info: my_node_info?.stun_info
      },
    }, ...(props.curNetworkInst.detail?.peer_route_pairs || [])]
  }

  return []
})

function routeCost(info: any) {
  if (info.route) {
    const cost = info.route.cost
    return cost ? cost === 1 ? 'p2p' : `relay(${cost})` : t('status.local')
  }

  return '?'
}

function resolveObjPath(path: string, obj: any = globalThis, separator = '.') {
  const properties = path.split(separator)
  return properties.reduce((prev, curr) => prev?.[curr], obj)
}

function statsCommon(info: any, field: string): number | undefined {
  if (!info.peer)
    return undefined

  let sum = 0
  let hasValue = false
  for (const conn of peerConns(info)) {
    const value = numericValue(resolveObjPath(field, conn))
    if (value === undefined)
      continue

    sum += value
    hasValue = true
  }
  return hasValue ? sum : undefined
}

function humanFileSize(bytes: number, si = false, dp = 1) {
  const thresh = si ? 1000 : 1024

  if (Math.abs(bytes) < thresh)
    return `${bytes} B`

  const units = si
    ? ['kB', 'MB', 'GB', 'TB', 'PB', 'EB', 'ZB', 'YB']
    : ['KiB', 'MiB', 'GiB', 'TiB', 'PiB', 'EiB', 'ZiB', 'YiB']
  let u = -1
  const r = 10 ** dp

  do {
    bytes /= thresh
    ++u
  } while (Math.round(Math.abs(bytes) * r) / r >= thresh && u < units.length - 1)

  return `${bytes.toFixed(dp)} ${units[u]}`
}

function txBytes(info: PeerRoutePair) {
  const tx = statsCommon(info, 'stats.tx_bytes')
  return tx === undefined ? '--' : humanFileSize(tx)
}

function rxBytes(info: PeerRoutePair) {
  const rx = statsCommon(info, 'stats.rx_bytes')
  return rx === undefined ? '--' : humanFileSize(rx)
}

function version(info: PeerRoutePair) {
  return info.route.version === '' ? 'unknown' : info.route.version
}

function ipFormat(info: PeerRoutePair) {
  const ip = info.route.ipv4_addr
  if (typeof ip === 'string')
    return ip
  return ip ? ipv4InetToString(ip) : ''
}

function oneTunnelProto(tunnel?: TunnelInfo): string {
  if (!tunnel)
    return ''

  const local_addr = tunnel.local_addr
  let isIPv6 = false;
  if (local_addr?.url) {
    try {
      const urlObj = new URL(local_addr.url, 'http://dummy');
      // IPv6 addresses in URLs are enclosed in brackets and contain ':'
      isIPv6 = /^\[.*:.*\]$/.test(urlObj.hostname);
    } catch (e) {
      // fallback to original check if URL parsing fails
      isIPv6 = local_addr.url.indexOf('[') >= 0;
    }
  }
  if (isIPv6)
    return `${tunnel.tunnel_type}6`
  else
    return tunnel.tunnel_type
}

function tunnelProto(info: PeerRoutePair) {
  return [...new Set(peerConns(info).map(c => oneTunnelProto(c.tunnel)))].join(',')
}

const myNodeInfo = computed(() => {
  if (!props.curNetworkInst)
    return {} as NodeInfo

  return props.curNetworkInst.detail?.my_node_info
})

interface Chip {
  label: string
  icon: string
}

const myNodeInfoChips = computed(() => {
  if (!props.curNetworkInst)
    return []

  const chips: Array<Chip> = []
  const my_node_info = props.curNetworkInst.detail?.my_node_info
  if (!my_node_info)
    return chips

  // peer id
  chips.push({
    label: `Peer ID: ${my_node_info.peer_id}`,
    icon: '',
  } as Chip)

  // TUN Device Name
  const dev_name = props.curNetworkInst.detail?.dev_name
  if (dev_name) {
    chips.push({
      label: `TUN Device Name: ${dev_name}`,
      icon: '',
    } as Chip)
  }

  // virtual ipv4
  chips.push({
    label: `Virtual IPv4: ${ipv4InetToString(my_node_info.virtual_ipv4)}`,
    icon: '',
  } as Chip)

  // local ipv4s
  const local_ipv4s = my_node_info.ips?.interface_ipv4s
  for (const [idx, ip] of local_ipv4s?.entries() ?? []) {
    chips.push({
      label: `Local IPv4 ${idx}: ${ipv4ToString(ip)}`,
      icon: '',
    } as Chip)
  }

  // local ipv6s
  const local_ipv6s = my_node_info.ips?.interface_ipv6s
  for (const [idx, ip] of local_ipv6s?.entries() ?? []) {
    chips.push({
      label: `Local IPv6 ${idx}: ${ipv6ToString(ip)}`,
      icon: '',
    } as Chip)
  }

  // public ip
  const public_ip = my_node_info.ips?.public_ipv4
  if (public_ip) {
    chips.push({
      label: `Public IP: ${ipv4ToString(public_ip)}`,
      icon: '',
    } as Chip)
  }

  const public_ipv6 = my_node_info.ips?.public_ipv6
  if (public_ipv6) {
    chips.push({
      label: `Public IPv6: ${ipv6ToString(public_ipv6)}`,
      icon: '',
    } as Chip)
  }

  // listeners:
  const listeners = my_node_info.listeners
  for (const [idx, listener] of listeners?.entries() ?? []) {
    chips.push({
      label: `Listener ${idx}: ${listener.url}`,
      icon: '',
    } as Chip)
  }

  const udpNatType = udpNatTypeName(my_node_info.stun_info)
  if (udpNatType) {
    chips.push({
      label: `UDP NAT Type: ${udpNatType}`,
      icon: '',
    } as Chip)
  }

  return chips
})

function globalSumCommon(field: string) {
  let sum = 0
  if (!peerRouteInfos.value)
    return sum

  for (const info of peerRouteInfos.value) {
    const tx = statsCommon(info, field)
    if (tx)
      sum += tx
  }
  return sum
}

function txGlobalSum() {
  return globalSumCommon('stats.tx_bytes')
}

function rxGlobalSum() {
  return globalSumCommon('stats.rx_bytes')
}

function natType(info: PeerRoutePair): string {
  return udpNatTypeName(info.route?.stun_info)
}

function isPublicServerRoute(info: PeerRoutePair): boolean {
  return info.route?.feature_flag?.is_public_server ?? false
}

function shouldAvoidRelayData(info: PeerRoutePair): boolean {
  return info.route?.feature_flag?.avoid_relay_data ?? false
}

const peerCount = computed(() => {
  if (!peerRouteInfos.value)
    return 0

  return peerRouteInfos.value.length
})

// calculate tx/rx rate every 2 seconds
let rateIntervalId = 0
const rateInterval = 2000
let prevTxSum = 0
let prevRxSum = 0
const txRate = ref('0')
const rxRate = ref('0')

// 控制节点详细信息chips的显示/隐藏
const showNodeDetails = ref(false)

// Multi-exit VPN live status (only when the connected kernel implements the
// fork's GetVpnStatus RPC; official kernels simply show no VPN section).
const vpnStatus = ref<GetVpnStatusResponse | undefined>()
let vpnStatusTimer: number | undefined

async function loadVpnStatus() {
  const instanceId = props.curNetworkInst?.instance_id
  if (!instanceId || !props.api.get_vpn_status) return
  try {
    vpnStatus.value = await props.api.get_vpn_status(instanceId)
  } catch (error) {
    console.warn('Failed to load vpn status', error)
    vpnStatus.value = undefined
  }
}

onMounted(() => {
  vpnStatusTimer = window.setInterval(() => { loadVpnStatus() }, 2000)
  loadVpnStatus()
})

onUnmounted(() => {
  if (vpnStatusTimer) clearInterval(vpnStatusTimer)
})

onMounted(() => {
  rateIntervalId = window.setInterval(() => {
    const curTxSum = txGlobalSum()
    txRate.value = humanFileSize((curTxSum - prevTxSum) / (rateInterval / 1000))
    prevTxSum = curTxSum

    const curRxSum = rxGlobalSum()
    rxRate.value = humanFileSize((curRxSum - prevRxSum) / (rateInterval / 1000))
    prevRxSum = curRxSum
  }, rateInterval)
})

onUnmounted(() => {
  clearInterval(rateIntervalId)
})

const dialogVisible = ref(false)
const dialogContent = ref<any>('')
const dialogHeader = ref('event_log')
const vpnPortalVisible = ref(false)
const networkConfig = ref<NetworkConfig>()
const vpnPortalEnabled = computed(() => props.curNetworkInst?.running
  && networkConfig.value?.vpn_portal_config !== undefined
  && networkConfig.value.vpn_portal_config.enabled !== false)

watch(() => props.curNetworkInst?.instance_id, async (instanceId, _, onCleanup) => {
  let active = true
  vpnPortalVisible.value = false
  networkConfig.value = undefined
  if (!instanceId) return
  let refreshTimer: ReturnType<typeof setTimeout> | undefined
  onCleanup(() => {
    active = false
    clearTimeout(refreshTimer)
  })
  const refresh = async () => {
    try {
      const config = await props.api.get_network_config(instanceId)
      if (active) networkConfig.value = config
    } catch (error) {
      console.error('Failed to load WireGuard settings', error)
    } finally {
      if (active) refreshTimer = setTimeout(refresh, 10_000)
    }
  }
  await refresh()
}, { immediate: true })

function showEventLogs() {
  const detail = props.curNetworkInst?.detail
  if (!detail)
    return

  dialogContent.value = detail.events?.map((event: string) => JSON.parse(event)) ?? []
  dialogHeader.value = 'event_log'
  dialogVisible.value = true
}
</script>

<template>
  <div class="frontend-lib">
    <Dialog v-model:visible="dialogVisible" modal :header="t(dialogHeader)" class="w-full h-auto max-h-full"
      :baseZIndex="2000">
      <Timeline :value="dialogContent">
        <template #opposite="slotProps">
          <small class="text-surface-500 dark:text-surface-400">{{ useTimeAgo(Date.parse(slotProps.item.time))
          }}</small>
        </template>
        <template #content="slotProps">
          <HumanEvent :event="slotProps.item.event" />
        </template>
      </Timeline>
    </Dialog>

    <VpnPortalDialog v-if="vpnPortalVisible && curNetworkInst && vpnPortalEnabled" :key="curNetworkInst.instance_id"
      :instance="curNetworkInst" :api="api" :readonly="readonly" @close="vpnPortalVisible = false" />

    <Card v-if="curNetworkInst?.error_msg">
      <template #title>
        Run Network Error
      </template>
      <template #content>
        <div class="flex flex-col gap-y-5">
          <div class="text-red-500">
            {{ curNetworkInst.error_msg }}
          </div>
        </div>
      </template>
    </Card>

    <template v-else>
      <Card v-if="vpnStatus && (vpnStatus.enabled || vpnStatus.exit_enabled || vpnStatus.rules.length > 0)">
        <template #title>
          {{ t('vpn_status_title') }}
        </template>
        <template #content>
          <div class="flex w-full flex-col gap-y-3">
            <div class="flex flex-wrap gap-3 text-sm">
              <Tag :value="vpnStatus.enabled ? t('vpn_status_enabled') : t('vpn_status_disabled')"
                :severity="vpnStatus.enabled ? 'success' : 'secondary'" />
              <Tag v-if="vpnStatus.exit_enabled" :value="t('vpn_status_exit_offering')" severity="info" />
              <Tag :value="`${t('vpn_status_fail_mode')}: ${vpnStatus.fail_mode}`" severity="secondary" />
              <Tag v-if="vpnStatus.dns_protect" :value="t('vpn_dns_protect')" severity="contrast" />
              <Tag v-if="vpnStatus.ipv6_protect" :value="t('vpn_ipv6_protect')" severity="contrast" />
              <Tag :value="`${t('vpn_status_routes')}: ${vpnStatus.routes_active ? t('vpn_status_routes_active') : t('vpn_status_routes_inactive')} (${vpnStatus.routes_installed})`"
                severity="secondary" />
            </div>
            <DataTable v-if="vpnStatus.exits.length > 0" :value="vpnStatus.exits" size="small"
              data-key="name">
              <Column field="name" :header="t('vpn_exit_name')" />
              <Column field="node_addr" :header="t('vpn_exit_node')" />
              <Column field="state" :header="t('vpn_exit_state')">
                <template #body="slotProps">
                  <Tag :value="slotProps.data.state"
                    :severity="slotProps.data.state === 'up' ? 'success' : 'danger'" />
                </template>
              </Column>
              <Column field="latency_ms" :header="t('vpn_exit_latency')">
                <template #body="slotProps">
                  {{ slotProps.data.latency_ms >= 0 ? `${slotProps.data.latency_ms} ms` : '-' }}
                </template>
              </Column>
              <Column field="tx_packets" :header="t('vpn_exit_tx')" />
              <Column field="tx_bytes" :header="t('vpn_exit_tx_bytes')" />
            </DataTable>
            <DataTable v-if="vpnStatus.rules.length > 0" :value="vpnStatus.rules" size="small"
              data-key="cidr">
              <Column field="cidr" :header="t('vpn_rule_cidr')" />
              <Column field="action" :header="t('vpn_rule_action')" />
              <Column field="exit" :header="t('vpn_rule_exit')" />
              <Column field="fallback_exits" :header="t('vpn_rule_fallbacks')">
                <template #body="slotProps">
                  {{ (slotProps.data.fallback_exits || []).join(',') || '-' }}
                </template>
              </Column>
              <Column field="priority" :header="t('vpn_rule_priority')" />
              <Column field="effective_exit" :header="t('vpn_rule_effective')">
                <template #body="slotProps">
                  {{ slotProps.data.effective_exit || '-' }}
                </template>
              </Column>
              <Column field="packets" :header="t('vpn_rule_packets')" />
              <Column field="bytes" :header="t('vpn_rule_bytes')" />
            </DataTable>
          </div>
        </template>
      </Card>

      <Card>
        <template #title>
          {{ t('my_node_info') }}
        </template>
        <template #content>
          <div class="flex w-full flex-col gap-y-5">
            <div class="gap-4">
              <!-- 网络流量图表 -->
              <div class="w-full">
                <NetworkChart :upload-rate="txRate" :download-rate="rxRate" />
              </div>
            </div>

            <!-- 展开/收起节点详细信息的divider按钮 -->
            <div class="w-full">
              <Button @click="showNodeDetails = !showNodeDetails"
                :icon="showNodeDetails ? 'pi pi-chevron-up' : 'pi pi-chevron-down'"
                :label="showNodeDetails ? t('hide_node_details') : t('show_node_details')" severity="secondary" outlined
                class="w-full justify-center" size="small" />
            </div>

            <!-- 节点详细信息chips，根据showNodeDetails状态显示/隐藏 -->
            <div v-show="showNodeDetails" class="flex flex-row items-center flex-wrap w-full max-h-40 overflow-scroll">
              <Chip v-for="(chip, i) in myNodeInfoChips" :key="i" :label="chip.label" :icon="chip.icon"
                class="mr-2 mt-2 text-sm" />
            </div>

            <div v-if="myNodeInfo" class="m-0 flex flex-row justify-center gap-x-5 text-sm">
              <Button v-if="vpnPortalEnabled" severity="info" :label="t('vpn_portal_devices')" @click="vpnPortalVisible = true" />
              <Button severity="info" :label="t('show_event_log')" @click="showEventLogs" />
            </div>
          </div>
        </template>
      </Card>

      <Divider />

      <Card>
        <template #title>
          <div class="flex items-center gap-3">
            <div class="flex items-center gap-2">
              <span>{{ t('peer_info') }}</span>
            </div>
            <div class="flex items-center gap-1">
              <Badge :value="peerCount" severity="info"
                class="text-lg font-semibold px-2 py-1 rounded-full bg-blue-100 text-blue-800 dark:bg-blue-900 dark:text-blue-200" />
            </div>
          </div>
        </template>
        <template #content>
          <DataTable :value="peerRouteInfos" column-resize-mode="fit" table-class="w-full">
            <Column :header="t('device')">
              <template #body="slotProps">
                <div class="flex flex-col gap-1">
                  <div class="whitespace-nowrap">{{ ipFormat(slotProps.data) || '—' }}</div>
                  <div v-tooltip="slotProps.data.route.hostname">
                    <span>{{ slotProps.data.route.hostname || '—' }}</span>
                    <span v-if="slotProps.data.route.cost && isPublicServerRoute(slotProps.data)" class="ml-1 space-x-1">
                      <Tag severity="info" value="Info">
                        {{ t('status.server') }}
                      </Tag>
                      <Tag v-if="shouldAvoidRelayData(slotProps.data)" severity="warn" value="Warn">
                        {{ t('status.relay') }}
                      </Tag>
                    </span>
                  </div>
                </div>
              </template>
            </Column>
            <Column :field="routeCost" :header="t('route_cost')" />
            <Column :field="tunnelProto" :header="t('tunnel_proto')" />
            <Column :field="latencyMs" :header="t('latency')" />
            <Column :header="t('total_traffic')">
              <template #body="slotProps">
                <div class="whitespace-nowrap">
                  <div>{{ t('upload') }}: {{ txBytes(slotProps.data) }}</div>
                  <div>{{ t('download') }}: {{ rxBytes(slotProps.data) }}</div>
                </div>
              </template>
            </Column>
            <Column :header="t('estimated_bandwidth')">
              <template #body="slotProps">
                <div class="whitespace-nowrap">
                  <div>{{ t('upload') }}: {{ estimatedBandwidth(slotProps.data).upload }}</div>
                  <div>{{ t('download') }}: {{ estimatedBandwidth(slotProps.data).download }}</div>
                </div>
              </template>
            </Column>
            <Column :field="lossRate" :header="t('loss_rate')" />
            <Column :field="natType" :header="t('nat_type')" />
            <Column :header="t('status.version')">
              <template #body="slotProps">
                <span>{{ version(slotProps.data) }}</span>
              </template>
            </Column>
          </DataTable>
        </template>
      </Card>
    </template>
  </div>
</template>

<style lang="postcss" scoped>
.p-timeline :deep(.p-timeline-event-opposite) {
  @apply flex-none;
}

:deep(.p-datatable .p-datatable-column-title) {
  white-space: nowrap;
}
</style>
