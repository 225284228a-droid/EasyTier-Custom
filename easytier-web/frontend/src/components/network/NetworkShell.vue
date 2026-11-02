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

const sections = computed(() => [
    { name: 'meshNetwork', label: t('web.mesh_network.section_overview'), icon: 'pi pi-globe' },
    { name: 'meshMembers', label: t('web.mesh_network.section_members'), icon: 'pi pi-server' },
    { name: 'meshSettings', label: t('web.mesh_network.section_settings'), icon: 'pi pi-shield', central: true },
    { name: 'meshAdvanced', label: t('web.mesh_network.section_advanced'), icon: 'pi pi-sliders-h', central: true },
    { name: 'meshPreset', label: t('web.mesh_network.section_preset'), icon: 'pi pi-box', central: true },
    { name: 'meshTemporary', label: t('web.mesh_network.section_temporary'), icon: 'pi pi-clock', central: true },
]);
const temporaryCount = computed(() => network.value?.members.filter(member => member.credential).length ?? 0);
</script>

<template>
    <div class="console-page">
        <header class="page-heading">
            <div class="min-w-0">
                <RouterLink :to="{ name: 'networkList' }" class="entity-link text-xs"><i class="pi pi-arrow-left" aria-hidden="true"></i>{{ t('web.main.network_list') }}</RouterLink>
                <h1 class="network-title"><i class="pi pi-globe" aria-hidden="true"></i><span>{{ network?.label ?? networkName }}</span>
                    <span v-if="network && network.label !== networkName" class="mono-value muted">{{ networkName }}</span>
                    <Tag v-if="view?.auto_join" :value="t('web.mesh_network.auto_join_on')" severity="info" /></h1>
            </div>
        </header>
        <Message v-if="viewError" severity="warn" :closable="false">{{ viewError }}</Message>
        <div class="network-shell">
            <nav class="network-sections" :aria-label="t('web.mesh_network.sections')">
                <template v-for="section in sections" :key="section.name">
                    <RouterLink v-if="!section.central || centralEnabled" :to="{ name: section.name, params: { networkName } }"
                        class="network-section" :class="{ active: route.name === section.name }"
                        :aria-current="route.name === section.name ? 'page' : undefined">
                        <i :class="section.icon" aria-hidden="true"></i><span>{{ section.label }}</span>
                        <span v-if="section.name === 'meshMembers' && network" class="nav-badge">{{ network.members.length }}</span>
                        <span v-if="section.name === 'meshTemporary' && temporaryCount" class="nav-badge">{{ temporaryCount }}</span>
                    </RouterLink>
                    <span v-else class="network-section disabled" v-tooltip.right="t('web.mesh_network.needs_central')">
                        <i :class="section.icon" aria-hidden="true"></i><span>{{ section.label }}</span>
                    </span>
                </template>
            </nav>
            <div class="network-section-content">
                <RouterView v-slot="{ Component }">
                    <component :is="Component" />
                </RouterView>
            </div>
        </div>
    </div>
</template>

<style scoped>
.network-title { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; overflow-wrap: anywhere; }
.network-shell { display: grid; grid-template-columns: 184px minmax(0, 1fr); gap: 16px; align-items: start; }
.network-sections {
    position: sticky; top: 0; display: flex; flex-direction: column; gap: 2px; padding: 8px;
    border: 1px solid var(--console-border); border-radius: 8px; background: var(--console-panel);
}
.network-section {
    display: flex; align-items: center; gap: 10px; padding: 8px 10px; border-radius: 6px;
    font-size: 13px; color: var(--p-text-color); text-decoration: none;
}
.network-section:hover { background: var(--p-content-hover-background); }
.network-section.active { background: var(--p-highlight-background); color: var(--p-highlight-color); font-weight: 600; }
.network-section.disabled { opacity: .45; cursor: not-allowed; }
.network-section .nav-badge { margin-left: auto; }
.network-section-content { min-width: 0; display: flex; flex-direction: column; gap: 16px; }
@media (max-width: 900px) {
    .network-shell { grid-template-columns: minmax(0, 1fr); }
    .network-sections { position: static; flex-direction: row; overflow-x: auto; padding: 6px; }
    .network-section { flex-shrink: 0; }
}
</style>
