<script setup lang="ts">
import { useTimeAgo } from '@vueuse/core'
import { NetworkInstance, type NetworkConfig, type TunnelInfo, type NodeInfo, type PeerRoutePair, type GetVpnStatusResponse } from '../types/network'
import type { RemoteClient } from '../modules/api'
import { useI18n } from 'vue-i18n';
import { computed, onMounted, onUnmounted, ref, useId, watch } from 'vue';
import { ipv4InetToString, ipv4ToString, ipv6ToString } from '../modules/utils';
import { estimatedBandwidth, latencyMs, lossRate, numericValue, peerConns, tunnelProtocols, udpNatTypeName } from '../modules/statusDisplay';
import { Badge, DataTable, Column, Tag, Chip, Button, Dialog, Timeline, Divider, Card, Message } from 'primevue';
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
  return tunnelProtocols(info, conn => oneTunnelProto(conn.tunnel))
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

// 控制节点详细信息的显示/隐藏
const showNodeDetails = ref(false)
const nodeDetailsId = useId()

// Subnet exit VPN live status (only when the connected kernel implements the
// fork's GetVpnStatus RPC; official kernels simply show no VPN section).
const vpnStatus = ref<GetVpnStatusResponse | undefined>()

const vpnActivationError = computed(() => {
  const error = vpnStatus.value?.activation_error
  return typeof error === 'string' ? error.trim() : ''
})

watch([() => props.api, () => props.api.scope, () => props.curNetworkInst?.instance_id,
  () => props.curNetworkInst?.running], async ([api, scope, instanceId, running], _, onCleanup) => {
  vpnStatus.value = undefined
  if (!instanceId || !running || !api.get_vpn_status) return
  let active = true
  let refreshTimer: ReturnType<typeof setTimeout> | undefined
  onCleanup(() => {
    active = false
    clearTimeout(refreshTimer)
  })
  const current = () => active && api.scope === scope
  const refresh = async () => {
    try {
      const status = await api.get_vpn_status!(instanceId)
      if (current()) vpnStatus.value = status
    } catch (error) {
      if (current()) {
        console.warn('Failed to load vpn status', error)
        vpnStatus.value = undefined
      }
    } finally {
      if (current()) refreshTimer = setTimeout(refresh, 2000)
    }
  }
  await refresh()
}, { immediate: true })

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
                :aria-expanded="showNodeDetails" :aria-controls="nodeDetailsId"
                :icon="showNodeDetails ? 'pi pi-chevron-up' : 'pi pi-chevron-down'"
                :label="showNodeDetails ? t('hide_node_details') : t('show_node_details')" severity="secondary" outlined
                class="w-full justify-center" size="small" />
            </div>

            <div v-show="showNodeDetails" :id="nodeDetailsId" class="node-details">
              <div class="flex flex-row items-center flex-wrap w-full max-h-40 overflow-auto">
                <Chip v-for="(chip, i) in myNodeInfoChips" :key="i" :label="chip.label" :icon="chip.icon"
                  class="mr-2 mt-2 text-sm" />
              </div>

              <section v-if="vpnStatus" class="vpn-status" :aria-label="t('vpn_status_title')">
                <div class="vpn-status-summary">
                  <h3 class="vpn-status-title"><i class="pi pi-shield" aria-hidden="true"></i>{{ t('vpn_status_title') }}</h3>
                  <Tag :value="vpnStatus.enabled ? t('vpn_status_enabled') : t('vpn_status_disabled')"
                    :severity="vpnStatus.enabled ? 'success' : 'secondary'" />
                </div>
                <div class="vpn-route-summary">
                  <span>{{ t('vpn_status_routes') }}</span>
                  <Tag :value="t(vpnStatus.routes_active ? 'vpn_status_routes_active' : 'vpn_status_routes_inactive')"
                    :severity="vpnStatus.routes_active ? 'info' : 'secondary'" />
                  <span>{{ t('vpn_status_routes_count', { count: vpnStatus.routes_installed }) }}</span>
                </div>
                <Message v-if="vpnActivationError" severity="error" :closable="false" class="vpn-activation-error">
                  {{ vpnActivationError }}
                </Message>
                <div v-if="vpnStatus.rules.length" class="vpn-rules">
                  <div class="vpn-rule-heading" aria-hidden="true">
                    <span>{{ t('vpn_rule_cidr') }}</span><span>{{ t('vpn_rule_exit') }}</span><span>{{ t('vpn_rule_state') }}</span>
                  </div>
                  <ul class="vpn-rule-list">
                    <li v-for="rule in vpnStatus.rules" :key="rule.cidr" class="vpn-rule-row">
                      <div class="vpn-rule-destination">
                        <span class="vpn-rule-label">{{ t('vpn_rule_cidr') }}</span><span class="vpn-rule-address">{{ rule.cidr }}</span>
                      </div>
                      <div class="vpn-rule-exit">
                        <span class="vpn-rule-label">{{ t('vpn_rule_exit') }}</span><span class="vpn-rule-address">{{ rule.exit_addr || t('vpn_rule_exit_nodes') }}</span>
                      </div>
                      <div class="vpn-rule-state">
                        <span class="sr-only">{{ t('vpn_rule_state') }}</span>
                        <Tag :value="t(rule.state === 'up' ? 'vpn_exit_state_up' : 'vpn_exit_state_down')"
                          :severity="rule.state === 'up' ? 'success' : 'danger'" />
                      </div>
                    </li>
                  </ul>
                </div>
                <p v-else class="vpn-no-rules">{{ t('vpn_no_rules') }}</p>
              </section>
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
                <div v-tooltip="t('estimated_bandwidth_help')" class="whitespace-nowrap">
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

.node-details {
  display: flex;
  min-width: 0;
  flex-direction: column;
  gap: 1rem;
}

.vpn-status {
  min-width: 0;
  padding: 1rem;
  border: 1px solid var(--p-content-border-color);
  border-radius: var(--p-border-radius-lg, 0.75rem);
  font-size: 0.875rem;
}

.vpn-status-summary,
.vpn-route-summary,
.vpn-status-title {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 0.5rem;
}

.vpn-status-summary {
  justify-content: space-between;
}

.vpn-status-title {
  margin: 0;
  font-size: 0.9375rem;
  font-weight: 600;
}

.vpn-route-summary {
  margin-top: 0.75rem;
  color: var(--p-text-muted-color);
}

.vpn-activation-error,
.vpn-rules {
  margin-top: 1rem;
}

.vpn-rule-heading,
.vpn-rule-row {
  display: grid;
  grid-template-columns: minmax(0, 1.2fr) minmax(0, 1fr) minmax(5rem, auto);
  align-items: center;
  gap: 0.75rem;
}

.vpn-rule-heading {
  padding: 0 0 0.5rem;
  color: var(--p-text-muted-color);
  font-size: 0.75rem;
}

.vpn-rule-list {
  margin: 0;
  padding: 0;
  list-style: none;
}

.vpn-rule-row {
  padding: 0.75rem 0;
  border-top: 1px solid var(--p-content-border-color);
}

.vpn-rule-address {
  overflow-wrap: anywhere;
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
}

.vpn-rule-label {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip: rect(0, 0, 0, 0);
}

.vpn-no-rules {
  margin: 0.75rem 0 0;
  color: var(--p-text-muted-color);
}

@media (max-width: 640px) {
  .vpn-status { padding: 0.875rem; }
  .vpn-rule-heading { display: none; }
  .vpn-rule-row { grid-template-columns: minmax(0, 1fr) auto; gap: 0.5rem 0.75rem; }
  .vpn-rule-destination { grid-column: 1; grid-row: 1; }
  .vpn-rule-exit { grid-column: 1; grid-row: 2; }
  .vpn-rule-state { grid-column: 2; grid-row: 1 / 3; }
  .vpn-rule-label {
    position: static;
    display: block;
    width: auto;
    height: auto;
    overflow: visible;
    clip: auto;
    color: var(--p-text-muted-color);
    font-size: 0.75rem;
    margin-bottom: 0.125rem;
  }
}
</style>
