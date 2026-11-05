<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';
import { Message, Tag } from 'primevue';
import { useRoute } from 'vue-router';
import { useI18n } from 'vue-i18n';
import ApiClient, { type MeshNetworkView } from '../../modules/api';
import { useNetworkDiscovery } from '../../modules/networkDiscovery';
import { provideMeshNetwork } from '../../modules/meshNetworkContext';

// One network, one page: its sub-pages share the network recognized from
// the devices and what the console keeps for it.
const props = defineProps<{ api: ApiClient; centralEnabled?: boolean }>();
const { t } = useI18n();
const route = useRoute();
const discovery = useNetworkDiscovery();

const networkName = computed(() => String(route.params.networkName ?? ''));
const network = computed(() => discovery.networks.value?.find(entry => entry.name === networkName.value));
const centralEnabled = computed(() => !!props.centralEnabled);
const view = ref<MeshNetworkView>();
const viewError = ref<string>();

const reload = async () => {
    const tasks: Promise<unknown>[] = [discovery.refresh()];
    if (centralEnabled.value) {
        tasks.push(props.api.get_mesh_network(networkName.value)
            .then(result => { view.value = result; viewError.value = undefined; })
            .catch(error => {
                // A network can appear on devices before the console has seen it.
                if (error?.response?.status !== 404) viewError.value = error?.response?.data?.message ?? String(error);
                view.value = undefined;
            }));
    }
    await Promise.all(tasks);
};

provideMeshNetwork({ api: props.api, networkName, network, view, centralEnabled, reload });
onMounted(() => void reload());
watch(centralEnabled, enabled => { if (enabled) void reload(); });

const deviceCount = computed(() => new Set(network.value?.members.map(member => member.device.machine_id)).size);
</script>

<template>
    <div class="console-page">
        <header class="page-heading network-heading">
            <div class="network-identity">
                <span class="network-icon" aria-hidden="true"><i class="pi pi-globe"></i></span>
                <div class="min-w-0">
                    <h1 class="network-title"><span>{{ network?.label ?? networkName }}</span>
                        <Tag v-if="view?.auto_join" :value="t('web.mesh_network.auto_join_on')" severity="info" /></h1>
                    <p class="network-meta">
                        <span v-if="network && network.label !== networkName" class="mono-value">{{ networkName }}</span>
                        <span>{{ t('web.mesh_network.devices') }} {{ deviceCount }}</span>
                        <span>{{ t('web.mesh_network.running') }} {{ network?.runningCount ?? 0 }} / {{ network?.members.length ?? 0 }}</span>
                    </p>
                </div>
            </div>
        </header>
        <Message v-if="viewError" severity="warn" :closable="false">{{ viewError }}</Message>
        <div class="network-section-content">
            <RouterView v-slot="{ Component }">
                <component :is="Component" />
            </RouterView>
        </div>
    </div>
</template>

<style scoped>
.network-identity { display: flex; align-items: center; gap: 14px; min-width: 0; }
.network-icon {
    display: inline-flex; align-items: center; justify-content: center; flex: none; width: 42px; height: 42px;
    border: 1px solid var(--console-border); border-radius: 10px; background: var(--console-panel); color: var(--p-primary-color); font-size: 18px;
}
.network-title { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; overflow-wrap: anywhere; }
.network-meta { display: flex; flex-wrap: wrap; gap: 2px 14px; margin: 2px 0 0; font-size: 12px; color: var(--console-muted); }
.network-section-content { min-width: 0; display: flex; flex-direction: column; gap: 16px; }
</style>
