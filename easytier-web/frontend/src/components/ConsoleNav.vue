<script setup lang="ts">
import { computed } from 'vue';
import { useRoute } from 'vue-router';
import { useI18n } from 'vue-i18n';
import type { DiscoveredNetwork } from '../modules/networkDiscovery';
import { NETWORK_SECTIONS } from '../modules/meshNetworkContext';

// One navigation for the sidebar and the mobile drawer. The open network
// lists its own pages under it, so a network page needs no menu of its own.
const props = defineProps<{ networks: DiscoveredNetwork[]; centralEnabled: boolean }>();
// Any link counts, the current page's included, so the drawer can close.
const emit = defineEmits<{ navigate: [] }>();
const onClick = (event: MouseEvent) => {
    if ((event.target as Element | null)?.closest('a')) emit('navigate');
};
const { t } = useI18n();
const route = useRoute();

const workspace = computed(() => [
    { name: 'dashboard', label: t('web.main.dashboard'), icon: 'pi pi-chart-pie' },
    { name: 'deviceList', label: t('web.main.device_list'), icon: 'pi pi-server' },
]);

const openName = computed(() => route.params.networkName ? String(route.params.networkName) : undefined);
// A network opened before the scan found it still shows its pages.
const entries = computed(() => {
    const open = openName.value;
    if (!open || props.networks.some(network => network.name === open)) return props.networks;
    return [...props.networks, { identity: `name:${open}`, name: open, label: open, members: [], runningCount: 0 } as DiscoveredNetwork];
});

const sectionCount = (network: DiscoveredNetwork, section: string) => {
    if (section === 'meshMembers') return network.members.length;
    if (section === 'meshTemporary') return network.members.filter(member => member.credential).length || undefined;
    return undefined;
};
const current = (name: string) => route.name === name ? 'page' : undefined;
</script>

<template>
    <nav class="console-nav" :aria-label="t('web.console.navigation')" @click="onClick">
        <div class="nav-group" role="group" :aria-label="t('web.console.workspace')">
            <div class="nav-caption" aria-hidden="true">{{ t('web.console.workspace') }}</div>
            <RouterLink v-for="item in workspace" :key="item.name" :to="{ name: item.name }" class="nav-item"
                :class="{ active: route.name === item.name }" :aria-current="current(item.name)">
                <i :class="item.icon" aria-hidden="true"></i><span class="nav-label">{{ item.label }}</span>
            </RouterLink>
        </div>
        <div class="nav-group" role="group" :aria-label="t('web.console.nav_networks')">
            <div class="nav-caption" aria-hidden="true">{{ t('web.console.nav_networks') }}</div>
            <RouterLink :to="{ name: 'networkList' }" class="nav-item" :class="{ active: route.name === 'networkList' }"
                :aria-current="current('networkList')">
                <i class="pi pi-th-large" aria-hidden="true"></i><span class="nav-label">{{ t('web.console.all_networks') }}</span>
            </RouterLink>
            <div v-for="network in entries" :key="network.identity" class="nav-network" :class="{ open: network.name === openName }">
                <RouterLink :to="{ name: 'meshNetwork', params: { networkName: network.name } }" class="nav-item"
                    :aria-expanded="network.name === openName" :title="network.label">
                    <span class="nav-network-state" :class="{ running: network.runningCount > 0 }" aria-hidden="true"></span>
                    <span class="nav-label">{{ network.label }}</span>
                    <span class="nav-count" :aria-label="`${t('web.mesh_network.running')} ${network.runningCount} / ${network.members.length}`">
                        {{ network.runningCount }}/{{ network.members.length }}</span>
                </RouterLink>
                <div v-if="network.name === openName" class="nav-sections">
                    <template v-for="section in NETWORK_SECTIONS" :key="section.name">
                        <RouterLink v-if="!section.central || centralEnabled" :to="{ name: section.name, params: { networkName: network.name } }"
                            class="nav-item nav-subitem" :class="{ active: route.name === section.name }" :aria-current="current(section.name)">
                            <i :class="section.icon" aria-hidden="true"></i><span class="nav-label">{{ t(section.label) }}</span>
                            <span v-if="sectionCount(network, section.name)" class="nav-count">{{ sectionCount(network, section.name) }}</span>
                        </RouterLink>
                        <span v-else class="nav-item nav-subitem disabled" aria-disabled="true" v-tooltip.right="t('web.mesh_network.needs_central')">
                            <i :class="section.icon" aria-hidden="true"></i><span class="nav-label">{{ t(section.label) }}</span>
                        </span>
                    </template>
                </div>
            </div>
        </div>
    </nav>
</template>
