<script setup lang="ts">
import { computed, defineAsyncComponent, onMounted, onUnmounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { Utils } from 'easytier-frontend-lib'
import ApiClient from '../modules/api'
import { buildTopology, collectTopologySnapshotsWithRetry, isTopologyAuthError, TopologySnapshotCache } from '../modules/networkTopology'
import { TrafficTracker } from '../modules/topologyTraffic'
import { TopologyAvailability } from '../modules/topologyAvailability'
import { PersistentTopologyArchive } from '../modules/dashboardPersistence'

const NetworkTopologyGlobe = defineAsyncComponent(() => import('./NetworkTopologyGlobe.vue'))
const props = defineProps<{ api?: ApiClient }>()
const { t } = useI18n()
const machines = ref<Utils.DeviceInfo[]>([])
const loading = ref(false)
const error = ref('')
const topology = ref(buildTopology([], []))
const trafficTracker = new TrafficTracker()
const snapshotCache = new TopologySnapshotCache()
const onlineDeviceCount = computed(() => machines.value.length)
const persistenceKey = computed(() => props.api?.persistenceScope ?? '')
const availability = new TopologyAvailability(performance.now())
let archive: PersistentTopologyArchive | undefined
let authorizedDevices: Utils.DeviceInfo[] = []
let mounted = false
let timer: ReturnType<typeof setTimeout> | undefined
let warningTimer: ReturnType<typeof setTimeout> | undefined
let expiryTimer: ReturnType<typeof setTimeout> | undefined
let expiryDeadline: number | undefined
let lastFreshMachines = new Set<string>()
let generation = 0
let requestController: AbortController | undefined

function updateWarning() {
  clearTimeout(warningTimer)
  const time = performance.now()
  error.value = availability.listUnavailable(time) ? t('web.dashboard.load_failed')
    : availability.incomplete(time) ? t('web.dashboard.partial_failure') : ''
  const deadline = availability.nextDeadline(time)
  if (mounted && deadline !== undefined)
    warningTimer = setTimeout(updateWarning, Math.max(1, Math.ceil(deadline - time)))
}

function bufferedTopology(time: number, sampleTraffic: boolean) {
  const cached = snapshotCache.read(time, lastFreshMachines)
  const liveMachines = new Set(cached.snapshots.map(snapshot => snapshot.device.machine_id))
  const fallback = archive?.read(authorizedDevices)
    .filter(snapshot => !liveMachines.has(snapshot.device.machine_id)) ?? []
  const nextTopology = buildTopology(cached.devices, [...cached.snapshots, ...fallback],
    sampleTraffic ? trafficTracker : undefined, time)
  if (!sampleTraffic)
    trafficTracker.applyCachedRates(nextTopology.links, time)
  machines.value = cached.devices
  topology.value = nextTopology
}

function clearPrivateData() {
  archive?.clear()
  authorizedDevices = []
  lastFreshMachines.clear()
  snapshotCache.clear()
  trafficTracker.clear()
  machines.value = []
  topology.value = buildTopology([], [])
}

function clearExpiryTimer() {
  clearTimeout(expiryTimer)
  expiryTimer = undefined
  expiryDeadline = undefined
}

function scheduleExpiry() {
  const deadline = snapshotCache.nextExpiry()
  if (deadline === expiryDeadline)
    return
  clearExpiryTimer()
  if (!mounted || deadline === undefined)
    return
  expiryDeadline = deadline
  const expiryGeneration = generation
  expiryTimer = setTimeout(() => {
    expiryTimer = undefined
    expiryDeadline = undefined
    if (!mounted || expiryGeneration !== generation)
      return
    const time = performance.now()
    bufferedTopology(time, false)
    scheduleExpiry()
  }, Math.max(1, Math.ceil(deadline - performance.now())))
}

async function loadTopology() {
  if (!props.api || loading.value)
    return
  const api = props.api
  const requestGeneration = generation
  const controller = new AbortController()
  const isCurrent = () => mounted && requestGeneration === generation && api === props.api
    && !controller.signal.aborted
  requestController = controller
  const requestOptions = { timeout: 8_000, signal: controller.signal }
  clearTimeout(timer)
  loading.value = true
  try {
    let devices: Utils.DeviceInfo[] = []
    try {
      devices = (await api.list_machines(requestOptions)).map(Utils.buildDeviceInfo)
      if (!isCurrent())
        return
      snapshotCache.updateDevices(devices, performance.now())
      authorizedDevices = devices
      availability.updateDevices(devices, performance.now())
      archive?.update(devices, [])
      bufferedTopology(performance.now(), false)
      updateWarning()
    } catch (cause) {
      if (!isCurrent())
        return
      if (isTopologyAuthError(cause)) {
        clearPrivateData()
        availability.clear(performance.now())
        updateWarning()
        controller.abort()
        return
      }
      availability.failListing()
      updateWarning()
      console.warn('Failed to list topology machines', cause)
    }
    const pending = devices.filter(device => device.running_network_count > 0)
    const freshMachines = new Set<string>()
    // Bound concurrent remote RPC calls, and collect all networks per machine.
    await Promise.all(Array.from({ length: Math.min(4, pending.length) }, async () => {
      while (pending.length && isCurrent()) {
        const device = pending.shift()!
        try {
          const snapshots = await collectTopologySnapshotsWithRetry(
            device,
            () => api.collect_machine_network_info(device.machine_id, requestOptions),
            isCurrent,
          )
          if (isCurrent()) {
            const collectedAt = performance.now()
            const accepted = snapshotCache.updateSnapshots(
              device,
              snapshots.map(snapshot => ({ ...snapshot, collectedAt })),
              collectedAt,
            )
            if (accepted) {
              freshMachines.add(device.machine_id)
              availability.report(device.machine_id, collectedAt)
              archive?.update(authorizedDevices, snapshots)
              updateWarning()
            }
          }
        } catch (cause) {
          if (!isCurrent())
            return
          if (isTopologyAuthError(cause)) {
            clearPrivateData()
            availability.clear(performance.now())
            updateWarning()
            controller.abort()
            return
          }
          console.warn(`Failed to collect topology for ${device.hostname}`, cause)
        }
      }
    }))
    if (isCurrent()) {
      const time = performance.now()
      lastFreshMachines = new Set(freshMachines)
      bufferedTopology(time, true)
      updateWarning()
      scheduleExpiry()
    }
  } catch (cause) {
    console.error('Failed to load dashboard topology', cause)
    if (isCurrent()) {
      availability.failListing()
      updateWarning()
    }
  } finally {
    if (requestController === controller)
      requestController = undefined
    loading.value = false
    if (mounted && props.api)
      timer = setTimeout(loadTopology, requestGeneration === generation ? 2_000 : 0)
  }
}

watch(() => props.api, () => {
  generation++
  requestController?.abort()
  clearTimeout(timer)
  clearTimeout(warningTimer)
  clearExpiryTimer()
  availability.clear(performance.now())
  authorizedDevices = []
  archive = persistenceKey.value ? new PersistentTopologyArchive(persistenceKey.value) : undefined
  lastFreshMachines.clear()
  snapshotCache.clear()
  trafficTracker.clear()
  machines.value = []
  topology.value = buildTopology([], [])
  error.value = ''
  if (mounted && !loading.value)
    void loadTopology()
})

onMounted(() => {
  mounted = true
  archive = persistenceKey.value ? new PersistentTopologyArchive(persistenceKey.value) : undefined
  void loadTopology()
})
onUnmounted(() => {
  mounted = false
  generation++
  requestController?.abort()
  clearTimeout(timer)
  clearTimeout(warningTimer)
  clearExpiryTimer()
  lastFreshMachines.clear()
  snapshotCache.clear()
  trafficTracker.clear()
  authorizedDevices = []
})
</script>

<template>
  <div class="dashboard">
    <div class="dashboard-summary">
      <div><span>{{ t('web.dashboard.devices') }}</span><strong>{{ onlineDeviceCount }}</strong></div>
      <div><span>{{ t('web.dashboard.networks') }}</span><strong>{{ topology.networkIdentities.length }}</strong></div>
      <div><span>{{ t('web.dashboard.connections') }}</span><strong>{{ topology.links.length }}</strong></div>
    </div>
    <p v-if="error" role="status" class="dashboard-error">{{ error }}</p>
    <NetworkTopologyGlobe :nodes="topology.nodes" :links="topology.links" :loading="loading"
      :persistence-key="persistenceKey"
      @refresh="loadTopology" />
  </div>
</template>

<style scoped>
.dashboard { min-width: 0; }
.dashboard-summary { display: flex; gap: 40px; padding: 8px 0 20px; margin-bottom: 20px; border-bottom: 1px solid var(--p-content-border-color); }
.dashboard-summary > div { display: flex; flex-direction: column; gap: 6px; min-width: 0; }
.dashboard-summary span { font-size: 12px; color: var(--p-text-muted-color); }
.dashboard-summary strong { font-size: 26px; font-weight: 600; }
.dashboard-error { font-size: 13px; color: var(--p-orange-600); }
@media (max-width: 480px) { .dashboard-summary { justify-content: space-between; gap: 12px; } }
</style>
