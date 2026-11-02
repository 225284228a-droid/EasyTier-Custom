<script setup lang="ts">
import { NetworkTypes, Utils, Api, RemoteManagement } from 'easytier-frontend-lib';
import { computed } from 'vue';
import ApiClient from '../modules/api';

// The device's configurations, shown in the device popup over any page.
const props = defineProps<{
    api: ApiClient;
    deviceId: string;
    deviceList: Array<Utils.DeviceInfo> | undefined;
    /** Instances locked against this console's batch edits; unset offers no lock. */
    lockedInstances?: string[];
}>();

const instanceId = defineModel<string | undefined>('instanceId');
const emits = defineEmits(['update', 'toggle-lock']);

const deviceInfo = computed<Utils.DeviceInfo | undefined | null>(() => {
    return props.deviceId ? props.deviceList?.find((device) => device.machine_id === props.deviceId) : null;
});

const remoteClient = computed<Api.RemoteClient>(() => props.api.get_remote_client(props.deviceId));

const newConfigGenerator = () => {
    const config = NetworkTypes.DEFAULT_NETWORK_CONFIG();
    config.hostname = deviceInfo.value?.hostname;
    return config;
}

</script>

<template>
    <RemoteManagement :api="remoteClient" v-model:instance-id="instanceId"
        :new-config-generator="newConfigGenerator" :locked-instances="lockedInstances"
        @update="emits('update')" @toggle-lock="(id: string, locked: boolean) => emits('toggle-lock', id, locked)" />
</template>

<style scoped>
.device-management {
    height: 100%;
    display: flex;
    flex-direction: column;
}

.network-content {
    flex: 1;
    overflow-y: auto;
}

/* 按钮样式 */
.button-container {
    gap: 0.5rem;
}

.create-button {
    font-weight: 600;
    min-width: 3rem;
}

/* 菜单样式定制 */
:deep(.p-menu) {
    min-width: 12rem;
    box-shadow: 0 0.5rem 1rem rgba(0, 0, 0, 0.15);
    padding: 0.25rem;
}

:deep(.p-menu .p-menuitem) {
    border-radius: 0.25rem;
}

:deep(.p-menu .p-menuitem-link) {
    padding: 0.65rem 1rem;
    font-size: 0.9rem;
}

:deep(.p-menu .p-menuitem-icon) {
    margin-right: 0.75rem;
}

:deep(.p-menu .p-menuitem.p-error .p-menuitem-text,
    .p-menu .p-menuitem.p-error .p-menuitem-icon) {
    color: var(--red-500);
}

:deep(.p-menu .p-menuitem:hover.p-error .p-menuitem-link) {
    background-color: var(--red-50);
}

/* 按钮图标样式 */
:deep(.p-button-icon-only) {
    width: 2.5rem !important;
    padding: 0.5rem !important;
}

:deep(.p-button-icon-only .p-button-icon) {
    font-size: 1rem;
}

/* 网络选择相关样式 */
.network-label {
    white-space: nowrap;
}

:deep(.network-select-container) {
    max-width: 100%;
}

/* Responsive design for mobile devices */
@media (max-width: 768px) {
    .network-header {
        padding: 0.75rem;
    }

    .network-content {
        padding: 0.75rem;
    }

    /* 在小屏幕上缩短网络标签文本 */
    .network-label {
        font-size: 0.9rem;
    }
}
</style>