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
    <section class="console-panel">
        <div class="summary-strip">
            <div class="summary-stat"><div class="summary-label"><span>{{ t('web.mesh_network.devices') }}</span><i class="pi pi-server" aria-hidden="true"></i></div>
                <div class="summary-value">{{ new Set(members.map(member => member.device.machine_id)).size }}</div></div>
            <div class="summary-stat"><div class="summary-label"><span>{{ t('web.mesh_network.running') }}</span><i class="pi pi-play" aria-hidden="true"></i></div>
                <div class="summary-value">{{ network?.runningCount ?? 0 }}<span class="summary-total"> / {{ members.length }}</span></div></div>
            <div class="summary-stat"><div class="summary-label"><span>{{ t('web.mesh_network.nodes') }}</span><i class="pi pi-share-alt" aria-hidden="true"></i></div>
                <div class="summary-value">{{ networkTopology.nodes.length }}</div></div>
            <div class="summary-stat"><div class="summary-label"><span>{{ t('web.dashboard.connections') }}</span><i class="pi pi-arrows-h" aria-hidden="true"></i></div>
                <div class="summary-value">{{ networkTopology.links.length }}</div></div>
        </div>
    </section>
    <!-- The network's settings: a card of their own on the counters' columns. -->
    <section v-if="view" class="console-panel">
        <div class="summary-strip overview-facts">
            <div class="summary-stat"><div class="summary-label"><span>{{ t('web.mesh_network.secure_mode') }}</span><i class="pi pi-shield" aria-hidden="true"></i></div>
                <div class="fact-value">{{ view.core.secure_mode ? t('web.mesh_network.value_on') : t('web.mesh_network.value_off') }}</div></div>
            <div class="summary-stat"><div class="summary-label"><span>{{ t('web.mesh_network.encryption') }}</span><i class="pi pi-lock" aria-hidden="true"></i></div>
                <div class="fact-value">{{ view.core.disable_encryption ? t('web.mesh_network.value_off') : (view.core.encryption_algorithm || t('web.mesh_network.value_default')) }}</div></div>
            <div class="summary-stat"><div class="summary-label"><span>{{ t('web.mesh_network.auto_join') }}</span><i class="pi pi-sign-in" aria-hidden="true"></i></div>
                <div class="fact-value">{{ view.auto_join ? t('web.mesh_network.value_on') : t('web.mesh_network.value_off') }}</div></div>
            <div class="summary-stat"><div class="summary-label"><span>{{ t('initial_nodes') }}</span><i class="pi pi-sitemap" aria-hidden="true"></i></div>
                <div class="fact-value mono-value" :title="view.preset.peers.join(', ')">{{ view.preset.peers.join(', ') || '—' }}</div></div>
        </div>
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
.overview-facts .summary-stat { min-width: 0; padding-top: 14px; padding-bottom: 14px; }
.fact-value { margin-top: 6px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 14px; font-weight: 550; line-height: 20px; }
.fact-value.mono-value { font-size: 13px; }
.mesh-globe { min-width: 0; padding: 16px 20px 20px; }
.dashboard-error { margin: 0 0 16px; font-size: 13px; color: var(--p-orange-600); overflow-wrap: anywhere; }
</style>
