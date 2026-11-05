<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { Button, InputSwitch, Message, Skeleton, useToast } from 'primevue';
import { useI18n } from 'vue-i18n';
import { NetworkTypes, UrlListInput } from 'easytier-frontend-lib';
import type { MeshPreset } from '../../modules/api';
import { failureMessage } from '../../modules/networkDiscovery';
import { useMeshNetwork } from '../../modules/meshNetworkContext';
import AdvancedFieldsEditor from './AdvancedFieldsEditor.vue';

const { t } = useI18n();
const toast = useToast();
const { api, networkName, view, reload } = useMeshNetwork();
const protos = NetworkTypes.PEER_PROTOCOLS;

const presetOf = (): MeshPreset | undefined => view.value && {
    peers: [...view.value.preset.peers],
    listeners: [...view.value.preset.listeners],
    advanced: { ...view.value.preset.advanced },
};
const preset = ref<MeshPreset>();
watch(() => JSON.stringify(view.value?.preset), () => { preset.value = presetOf(); }, { immediate: true });
const changed = computed(() => !!preset.value && JSON.stringify(preset.value) !== JSON.stringify(presetOf()));

// Unset advanced values run with the core's default and are not stored.
const advanced = computed({
    get: () => preset.value?.advanced ?? {},
    set: (values: Record<string, any>) => {
        if (!preset.value) return;
        preset.value.advanced = Object.fromEntries(Object.entries(values).filter(([, value]) => value !== null && value !== undefined));
    },
});

const saving = ref(false);
const save = async () => {
    if (!preset.value) return;
    saving.value = true;
    try {
        view.value = await api.save_mesh_preset(networkName.value, preset.value);
        toast.add({ severity: 'success', summary: t('web.mesh_network.section_preset'), detail: t('web.device_management.config_saved'), life: 2000 });
    } catch (error) {
        toast.add({ severity: 'error', summary: t('web.mesh_network.section_preset'), detail: failureMessage(error), life: 5000 });
    } finally {
        saving.value = false;
    }
};

const togglingAutoJoin = ref(false);
const setAutoJoin = async (enabled: boolean) => {
    togglingAutoJoin.value = true;
    try {
        view.value = await api.set_mesh_auto_join(networkName.value, enabled);
        await reload();
    } catch (error) {
        toast.add({ severity: 'error', summary: t('web.mesh_network.auto_join'), detail: failureMessage(error), life: 5000 });
    } finally {
        togglingAutoJoin.value = false;
    }
};
</script>

<template>
    <section class="console-panel">
        <header class="section-heading"><h2>{{ t('web.mesh_network.auto_join') }}</h2></header>
        <div class="preset-body">
            <div v-if="!view" class="loading-rows"><Skeleton height="2rem" /></div>
            <div v-else class="field-row">
                <InputSwitch input-id="preset-auto-join" :model-value="view.auto_join" :disabled="togglingAutoJoin" @update:model-value="setAutoJoin" />
                <label for="preset-auto-join">{{ t('web.mesh_network.auto_join_label') }}</label>
            </div>
            <p class="text-sm muted m-0">{{ t('web.mesh_network.auto_join_hint') }}</p>
        </div>
    </section>
    <section class="console-panel">
        <header class="section-heading preset-heading">
            <h2>{{ t('web.mesh_network.section_preset') }}</h2>
            <div class="flex gap-2 ml-auto">
                <Button :label="t('web.mesh_network.discard')" icon="pi pi-undo" text severity="secondary" size="small" :disabled="!changed" @click="preset = presetOf()" />
                <Button :label="t('web.common.save')" icon="pi pi-check" size="small" :loading="saving" :disabled="!changed" @click="save" />
            </div>
        </header>
        <p class="section-hint">{{ t('web.mesh_network.preset_hint') }}</p>
        <div v-if="!preset" class="preset-body loading-rows"><Skeleton v-for="i in 3" :key="i" height="2rem" /></div>
        <div v-else class="preset-body">
            <Message severity="secondary" size="small" :closable="false">{{ t('web.mesh_network.preset_core_hint') }}</Message>
            <div class="field">
                <div class="flex items-center gap-2">
                    <label for="preset-peers">{{ t('initial_nodes') }}</label>
                    <span class="pi pi-question-circle text-sm" v-tooltip="t('initial_nodes_help')"></span>
                </div>
                <UrlListInput id="preset-peers" v-model="preset.peers" :protos="protos" default-url="tcp://:11010"
                    :add-label="t('add_initial_node')" :placeholder="t('initial_node_placeholder')" />
            </div>
            <div class="field">
                <label for="preset-listeners">{{ t('web.mesh_network.listeners') }}</label>
                <UrlListInput id="preset-listeners" v-model="preset.listeners" :protos="protos" default-url="tcp://0.0.0.0:11010"
                    :add-label="t('web.common.add')" placeholder="tcp://0.0.0.0:11010" />
            </div>
            <AdvancedFieldsEditor v-model="advanced" mode="preset" :disabled="saving" />
        </div>
    </section>
</template>

<style scoped>
.preset-heading { flex-wrap: wrap; }
.section-hint { margin: 0; padding: 0 20px 4px; font-size: 13px; color: var(--p-text-muted-color); }
.preset-body { display: flex; flex-direction: column; gap: 16px; padding: 12px 20px 20px; }
.field { display: flex; flex-direction: column; gap: 6px; max-width: 44rem; }
.field-row { display: flex; align-items: center; gap: 10px; }
</style>
