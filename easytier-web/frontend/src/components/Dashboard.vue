<script setup lang="ts">
import { computed, defineAsyncComponent, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { Utils } from 'easytier-frontend-lib'
import ApiClient from '../modules/api'
import { buildTopology, type NetworkSnapshot } from '../modules/networkTopology'

const NetworkTopologyGlobe = defineAsyncComponent(() => import('./NetworkTopologyGlobe.vue'))
const props = defineProps<{ api?: ApiClient }>()
const { t } = useI18n()
const machines = ref<Utils.DeviceInfo[]>([])
const snapshots = ref<NetworkSnapshot[]>([])
const loading = ref(false)
const error = ref('')
const topology = computed(() => buildTopology(machines.value, snapshots.value))
const onlineDeviceCount = computed(() => machines.value.length)
let mounted = false
let timer: ReturnType<typeof setTimeout> | undefined

async function loadTopology() {
  if (!props.api || loading.value)
    return
  clearTimeout(timer)
  loading.value = true
  error.value = ''
  try {
    const devices = (await props.api.list_machines()).map(Utils.buildDeviceInfo)
    const pending = devices.filter(device => device.running_network_count > 0)
    const nextSnapshots: NetworkSnapshot[] = []
    let failed = false
    // Bound concurrent remote RPC calls, and collect all networks per machine.
    await Promise.all(Array.from({ length: Math.min(4, pending.length) }, async () => {
      while (pending.length) {
        const device = pending.shift()!
        try {
          const infos = await props.api!.collect_machine_network_info(device.machine_id)
          for (const [instanceId, detail] of Object.entries(infos)) {
            if (detail?.running)
              nextSnapshots.push({ device, instanceId, detail })
          }
        } catch (error) {
          failed = true
          console.warn(`Failed to collect topology for ${device.hostname}`, error)
        }
      }
    }))
    if (mounted) {
      machines.value = devices
      snapshots.value = nextSnapshots
      error.value = failed ? t('web.dashboard.partial_failure') : ''
    }
  } catch (cause) {
    console.error('Failed to load dashboard topology', cause)
    if (mounted)
      error.value = t('web.dashboard.load_failed')
  } finally {
    loading.value = false
    if (mounted)
      timer = setTimeout(loadTopology, 10_000)
  }
}

onMounted(() => {
  mounted = true
  void loadTopology()
})
onUnmounted(() => {
  mounted = false
  clearTimeout(timer)
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
