<script setup lang="ts">
import { computed, ref } from 'vue';
import { Button, Message, Skeleton, useToast } from 'primevue';
import { useI18n } from 'vue-i18n';
import { failureMessage } from '../../modules/networkDiscovery';
import { useMeshNetwork } from '../../modules/meshNetworkContext';
import TemporaryDevices from './TemporaryDevices.vue';

const { t } = useI18n();
const toast = useToast();
const { api, networkName, network, view, reload } = useMeshNetwork();

const centralId = computed(() => network.value?.central?.network_id);
const memberDevices = computed(() => (network.value?.members ?? [])
    .filter(member => !member.credential).map(member => member.device.machine_id));
const autoAdd = ref(false);
const creating = ref(false);

// Temporary devices need a central record: the console publishes their
// whole configuration and runs the gateway. It is created from the
// network's core settings the first time a temporary device is added.
const addFirst = async () => {
    if (!view.value) return;
    creating.value = true;
    try {
        const gateway = await api.get_gateway_info().catch(() => ({ enabled: false } as { enabled: boolean; peer_url?: string }));
        const peers = view.value.preset.peers.filter(peer => peer !== gateway.peer_url);
        await api.create_network({
            display_name: view.value.core.display_name ?? '',
            network_name: networkName.value,
            networking_method: gateway.enabled ? 'Gateway' : peers.length ? 'Manual' : 'Standalone',
            public_server_url: null,
            peer_urls: gateway.enabled ? [] : peers,
            virtual_cidr: null,
            secure_mode: true,
        }, view.value.core.network_secret ?? undefined);
        autoAdd.value = true;
        await reload();
    } catch (error) {
        toast.add({ severity: 'error', summary: t('web.mesh_network.add_temporary'), detail: failureMessage(error), life: 5000 });
    } finally {
        creating.value = false;
    }
};
</script>

<template>
    <TemporaryDevices v-if="centralId" :key="centralId" :api="api" :network-id="centralId" :auto-add="autoAdd"
        :member-devices="memberDevices" />
    <section v-else class="console-panel">
        <header class="section-heading"><h2>{{ t('web.mesh_network.section_temporary') }}</h2></header>
        <div v-if="!view" class="loading-rows"><Skeleton v-for="i in 2" :key="i" height="2rem" /></div>
        <div v-else class="console-empty-state">
            <i class="pi pi-clock" aria-hidden="true"></i>
            <h2>{{ t('web.mesh_network.temporary_empty') }}</h2>
            <p>{{ t('web.mesh_network.temporary_empty_hint') }}</p>
            <Message v-if="!view.core.secure_mode" severity="warn" :closable="false">
                {{ t('web.mesh_network.temporary_needs_secure') }}
                <RouterLink :to="{ name: 'meshSettings', params: { networkName } }" class="entity-link">{{ t('web.mesh_network.section_settings') }}</RouterLink>
            </Message>
            <Button v-else icon="pi pi-plus" :label="t('web.mesh_network.add_temporary')" :loading="creating" @click="addFirst" />
        </div>
    </section>
</template>
