<script setup lang="ts">
import { computed, ref } from 'vue';
import { Button, Column, DataTable, Dialog, Message, Skeleton, useToast } from 'primevue';
import { useI18n } from 'vue-i18n';
import { Utils } from 'easytier-frontend-lib';
import { failureMessage } from '../../modules/networkDiscovery';
import { useMeshNetwork } from '../../modules/meshNetworkContext';
import TemporaryDevices from './TemporaryDevices.vue';
import CredentialTtlPicker from './CredentialTtlPicker.vue';
import { DAY } from '../../modules/credentialTtl';

const { t } = useI18n();
const toast = useToast();
const { api, networkName, network, view, reload } = useMeshNetwork();

const centralId = computed(() => network.value?.central?.network_id);
const memberDevices = computed(() => (network.value?.members ?? [])
    .filter(member => !member.credential).map(member => member.device.machine_id));

// Temporary devices need a central record: the console publishes their
// whole configuration and runs the gateway. The record is created only when
// the first temporary devices are confirmed, so a cancelled dialog leaves
// nothing behind.
const addVisible = ref(false);
const adding = ref(false);
const candidates = ref<Utils.DeviceInfo[]>();
const selected = ref<Utils.DeviceInfo[]>([]);
const ttl = ref(7 * DAY);

const openAdd = async () => {
    selected.value = [];
    ttl.value = 7 * DAY;
    candidates.value = undefined;
    addVisible.value = true;
    const members = new Set(memberDevices.value);
    try {
        candidates.value = (await api.list_machines()).map(Utils.buildDeviceInfo)
            .filter(device => !!device.machine_id && !members.has(device.machine_id));
    } catch (error) {
        console.error(error);
        candidates.value = [];
    }
};

const add = async () => {
    if (!view.value || !selected.value.length) return;
    adding.value = true;
    let created: string | undefined;
    try {
        const gateway = await api.get_gateway_info().catch(() => ({ enabled: false } as { enabled: boolean; peer_url?: string }));
        const peers = view.value.preset.peers.filter(peer => peer !== gateway.peer_url);
        created = (await api.create_network({
            display_name: view.value.core.display_name ?? '',
            network_name: networkName.value,
            networking_method: gateway.enabled ? 'Gateway' : peers.length ? 'Manual' : 'Standalone',
            public_server_url: null,
            peer_urls: gateway.enabled ? [] : peers,
            virtual_cidr: null,
            secure_mode: true,
        }, view.value.core.network_secret ?? undefined)).network_id;
        await api.add_network_members(created, selected.value.map(device => device.machine_id), true, ttl.value);
        created = undefined;
        addVisible.value = false;
        await reload();
    } catch (error) {
        // A record without its first devices is not kept.
        if (created) await api.delete_network(created).catch(console.error);
        toast.add({ severity: 'error', summary: t('web.mesh_network.add_temporary'), detail: failureMessage(error), life: 5000 });
    } finally {
        adding.value = false;
    }
};
</script>

<template>
    <TemporaryDevices v-if="centralId" :key="centralId" :api="api" :network-id="centralId" :member-devices="memberDevices" />
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
            <Button v-else icon="pi pi-plus" :label="t('web.mesh_network.add_temporary')" @click="openAdd" />
        </div>
    </section>

    <Dialog v-model:visible="addVisible" modal class="console-dialog" :header="t('web.network_detail.add_member')" :style="{ width: '32rem', maxWidth: 'calc(100vw - 32px)' }">
        <div v-if="candidates === undefined" class="loading-rows"><Skeleton v-for="i in 3" :key="i" height="2rem" /></div>
        <template v-else>
            <div v-if="!candidates.length" class="text-sm muted">{{ t('web.network_detail.no_candidate_devices') }}</div>
            <DataTable v-else v-model:selection="selected" :value="candidates" data-key="machine_id" size="small" scrollable
                scroll-height="20rem" selection-mode="multiple">
                <Column selection-mode="multiple" style="width: 3rem" />
                <Column field="hostname" :header="t('web.device.hostname')" />
                <Column field="easytier_version" :header="t('web.device.version')" />
            </DataTable>
        </template>
        <div class="flex flex-col gap-2 mt-2">
            <p class="text-sm muted m-0">{{ t('web.network_detail.add_temporary_hint') }}</p>
            <div class="flex items-start gap-2">
                <label class="text-sm shrink-0 pt-1.5">{{ t('web.network_detail.credential_ttl') }}</label>
                <CredentialTtlPicker v-model="ttl" />
            </div>
        </div>
        <template #footer>
            <Button :label="t('web.common.cancel')" icon="pi pi-times" text severity="secondary" @click="addVisible = false" />
            <Button :label="t('web.common.confirm')" icon="pi pi-check" :loading="adding" :disabled="!selected.length" @click="add" />
        </template>
    </Dialog>
</template>
