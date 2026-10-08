import { computed, type Ref, onMounted, onUnmounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { Utils } from 'easytier-frontend-lib'
import ApiClient from './api'
import { buildTopology, collectTopologySnapshotsWithRetry, isTopologyAuthError, TopologySnapshotCache } from './networkTopology'
import { TrafficTracker } from './topologyTraffic'
import { TopologyAvailability } from './topologyAvailability'
import { PersistentTopologyArchive } from './dashboardPersistence'

export function useMeshTopology(api: Readonly<Ref<ApiClient | undefined>>) {
  const { t } = useI18n()
  const machines = ref<Utils.DeviceInfo[]>([])
  const loading = ref(false)
  const manualRefreshing = ref(false)
  const error = ref('')
  const topology = ref(buildTopology([], []))
  const trafficTracker = new TrafficTracker()
  const snapshotCache = new TopologySnapshotCache()
  const onlineDeviceCount = computed(() => machines.value.length)
  const persistenceKey = computed(() => api.value?.persistenceScope ?? '')
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

  async function loadTopology(manual = false) {
    if (!api.value)
      return
    if (loading.value) {
      if (manual)
        manualRefreshing.value = true
      return
    }
    const client = api.value
    const requestGeneration = generation
    const requestScope = persistenceKey.value
    const controller = new AbortController()
    const isCurrent = () => mounted && requestGeneration === generation && client === api.value
      && requestScope === persistenceKey.value && !controller.signal.aborted
    requestController = controller
    const requestOptions = { timeout: 8_000, signal: controller.signal }
    clearTimeout(timer)
    loading.value = true
    manualRefreshing.value = manual
    try {
      let devices: Utils.DeviceInfo[] = []
      try {
        devices = (await client.list_machines(requestOptions)).map(Utils.buildDeviceInfo)
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
      const pending = devices.filter(device => device.online !== false && device.running_network_count > 0)
      const freshMachines = new Set<string>()
      // Bound concurrent remote RPC calls, and collect all networks per machine.
      await Promise.all(Array.from({ length: Math.min(4, pending.length) }, async () => {
        while (pending.length && isCurrent()) {
          const device = pending.shift()!
          try {
            const snapshots = await collectTopologySnapshotsWithRetry(
              device,
              () => client.collect_machine_network_info(device.machine_id, requestOptions),
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
      if (requestController !== controller) return
      requestController = undefined
      loading.value = false
      manualRefreshing.value = false
      if (mounted && api.value)
        timer = setTimeout(loadTopology, requestGeneration === generation ? 2_000 : 0)
    }
  }

  watch([api, persistenceKey], () => {
    generation++
    requestController?.abort()
    requestController = undefined
    loading.value = false
    manualRefreshing.value = false
    clearTimeout(timer)
    clearTimeout(warningTimer)
    clearExpiryTimer()
    availability.clear(performance.now())
    authorizedDevices = []
    archive?.clear()
    archive = persistenceKey.value ? new PersistentTopologyArchive(persistenceKey.value) : undefined
    lastFreshMachines.clear()
    snapshotCache.clear()
    trafficTracker.clear()
    machines.value = []
    topology.value = buildTopology([], [])
    error.value = ''
    if (mounted)
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

  return { machines, loading, manualRefreshing, error, topology, onlineDeviceCount, persistenceKey, loadTopology }
}
