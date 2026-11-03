<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { Drawer, useToast } from 'primevue';
import { useRoute, useRouter } from 'vue-router';
import { useI18n } from 'vue-i18n';
import { Utils } from 'easytier-frontend-lib';
import ApiClient from '../modules/api';
import { failureMessage, useNetworkDiscovery } from '../modules/networkDiscovery';
import DeviceManagement from './DeviceManagement.vue';

// Device management over the current page. The device lives in the query,
// so closing the popup leaves the page where it was.
const props = defineProps<{ api: ApiClient; centralEnabled?: boolean }>();
const { t } = useI18n();
const toast = useToast();
const route = useRoute();
const router = useRouter();
const discovery = useNetworkDiscovery();

const deviceId = computed(() => typeof route.query.device === 'string' ? route.query.device : undefined);
const instanceId = computed({
    get: () => typeof route.query.instance === 'string' ? route.query.instance : undefined,
    set: (instance: string | undefined) => {
        // Picking another network shows its status; only the first one opens for editing.
        void router.replace({ query: { ...route.query, instance: instance || undefined, edit: undefined } });
    },
});
const editOnOpen = computed(() => route.query.edit === '1');
const visible = computed({
    get: () => !!deviceId.value,
    set: (open: boolean) => {
        if (!open) void router.push({ query: { ...route.query, device: undefined, instance: undefined, edit: undefined } });
    },
});

const devices = ref<Utils.DeviceInfo[]>();
const lockedInstances = ref<string[]>();
const load = async () => {
    if (!deviceId.value) return;
    const device = deviceId.value;
    const [machines, members] = await Promise.all([
        props.api.list_machines().catch(() => undefined),
        props.centralEnabled ? props.api.list_mesh_members().catch(() => undefined) : Promise.resolve(undefined),
    ]);
    if (device !== deviceId.value) return;
    if (machines) devices.value = machines.map(Utils.buildDeviceInfo);
    lockedInstances.value = members
        ?.filter(member => member.device_id === device && member.locked)
        .map(member => member.instance_id) ?? (props.centralEnabled ? [] : undefined);
};
watch([deviceId, () => props.centralEnabled], () => void load(), { immediate: true });

const deviceName = computed(() => {
    const device = devices.value?.find(device => device.machine_id === deviceId.value);
    return device ? device.alias || device.hostname || device.machine_id : deviceId.value;
});

const toggleLock = async (instance: string, locked: boolean) => {
    if (!deviceId.value) return;
    try {
        await props.api.set_mesh_lock(deviceId.value, instance, locked);
        const others = (lockedInstances.value ?? []).filter(id => id !== instance);
        lockedInstances.value = locked ? [...others, instance] : others;
        toast.add({ severity: 'success', summary: t(locked ? 'advanced_lock' : 'advanced_unlock'), life: 2000 });
        void discovery.refresh();
    } catch (error) {
        toast.add({ severity: 'error', summary: t(locked ? 'advanced_lock' : 'advanced_unlock'), detail: failureMessage(error), life: 5000 });
    }
};
</script>

<template>
    <Drawer v-model:visible="visible" position="right" class="console-device-drawer" :base-z-index="1000">
        <template #header>
            <div class="drawer-heading flex items-baseline gap-3 min-w-0">
                <h2 class="truncate">{{ deviceName }}</h2>
                <p class="mono-value truncate">{{ deviceId }}</p>
            </div>
        </template>
        <DeviceManagement v-if="deviceId" :key="`${deviceId}:${instanceId ?? ''}`" v-model:instance-id="instanceId"
            :api="api" :device-id="deviceId" :device-list="devices" :locked-instances="lockedInstances" :edit-on-open="editOnOpen"
            @update="load" @toggle-lock="toggleLock" />
    </Drawer>
</template>
