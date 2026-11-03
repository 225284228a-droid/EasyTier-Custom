<script setup lang="ts">
import { computed, defineAsyncComponent } from 'vue';
import { useI18n } from 'vue-i18n';
import { useMeshTopology } from '../../modules/useMeshTopology';
import { networkNameIdentity } from '../../modules/networkTopology';
import { useMeshNetwork } from '../../modules/meshNetworkContext';

const NetworkTopologyGlobe = defineAsyncComponent(() => import('../NetworkTopologyGlobe.vue'));

const { t } = useI18n();
const { api, networkName, network, view } = useMeshNetwork();
const members = computed(() => network.value?.members ?? []);

// The shared topology, narrowed to this network.
const { topology, manualRefreshing, error: topologyError, persistenceKey, loadTopology } = useMeshTopology(computed(() => api));
const networkTopology = computed(() => {
    const identity = networkNameIdentity(networkName.value);
    const nodes = topology.value.nodes.filter(node => node.networkIdentity === identity);
    const ids = new Set(nodes.map(node => node.id));
    return { nodes, links: topology.value.links.filter(link => ids.has(link.source) && ids.has(link.target)) };
});
const globeKey = computed(() => `${persistenceKey.value}:network:${networkName.value}`);
</script>

<template>
    <section class="console-panel summary-strip">
        <div class="summary-stat"><div class="summary-label"><span>{{ t('web.mesh_network.devices') }}</span><i class="pi pi-server" aria-hidden="true"></i></div>
            <div class="summary-value">{{ new Set(members.map(member => member.device.machine_id)).size }}</div></div>
        <div class="summary-stat"><div class="summary-label"><span>{{ t('web.mesh_network.running') }}</span><i class="pi pi-play" aria-hidden="true"></i></div>
            <div class="summary-value">{{ network?.runningCount ?? 0 }}<span class="summary-total"> / {{ members.length }}</span></div></div>
        <div class="summary-stat"><div class="summary-label"><span>{{ t('web.mesh_network.nodes') }}</span><i class="pi pi-share-alt" aria-hidden="true"></i></div>
            <div class="summary-value">{{ networkTopology.nodes.length }}</div></div>
        <div class="summary-stat"><div class="summary-label"><span>{{ t('web.dashboard.connections') }}</span><i class="pi pi-arrows-h" aria-hidden="true"></i></div>
            <div class="summary-value">{{ networkTopology.links.length }}</div></div>
    </section>
    <section v-if="view" class="console-panel overview-facts">
        <div><span class="muted">{{ t('web.mesh_network.secure_mode') }}</span><strong>{{ view.core.secure_mode ? t('web.mesh_network.value_on') : t('web.mesh_network.value_off') }}</strong></div>
        <div><span class="muted">{{ t('web.mesh_network.encryption') }}</span><strong>{{ view.core.disable_encryption ? t('web.mesh_network.value_off') : (view.core.encryption_algorithm || t('web.mesh_network.value_default')) }}</strong></div>
        <div><span class="muted">{{ t('web.mesh_network.auto_join') }}</span><strong>{{ view.auto_join ? t('web.mesh_network.value_on') : t('web.mesh_network.value_off') }}</strong></div>
        <div class="min-w-0"><span class="muted">{{ t('initial_nodes') }}</span><strong class="mono-value truncate">{{ view.preset.peers.join(', ') || '—' }}</strong></div>
    </section>
    <section class="console-panel dashboard">
        <header class="section-heading"><h2>{{ t('web.mesh_network.topology') }}</h2></header>
        <div class="mesh-globe">
            <p v-if="topologyError" role="status" class="dashboard-error">{{ topologyError }}</p>
            <NetworkTopologyGlobe :nodes="networkTopology.nodes" :links="networkTopology.links" :loading="manualRefreshing"
                :persistence-key="globeKey" @refresh="loadTopology(true)" />
        </div>
    </section>
</template>

<style scoped>
.summary-total { font-size: 0.5em; color: var(--p-text-muted-color); }
.overview-facts { display: grid; grid-template-columns: repeat(auto-fill, minmax(200px, 1fr)); gap: 12px; padding: 14px 18px; }
.overview-facts > div { display: flex; flex-direction: column; gap: 2px; font-size: 13px; min-width: 0; }
.mesh-globe { min-width: 0; padding: 16px 20px 20px; }
.dashboard-error { margin: 0 0 16px; font-size: 13px; color: var(--p-orange-600); overflow-wrap: anywhere; }
</style>
