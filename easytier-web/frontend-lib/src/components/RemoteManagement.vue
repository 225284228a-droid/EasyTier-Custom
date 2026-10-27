<script setup lang="ts">
import { Button, ConfirmPopup, Divider, IftaLabel, Menu, Message, Select, Tag, useConfirm, useToast, type VirtualScrollerLazyEvent } from 'primevue';
import { computed, onMounted, onUnmounted, Ref, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import * as Api from '../modules/api';
import * as Utils from '../modules/utils';
import * as NetworkTypes from '../types/network';
import { type MenuItem } from 'primevue/menuitem';
import { ConfigApplyError, createConfigStore, supportsRevisionStore, type ConfigStore } from '../modules/configStore';
import { createConfigEditor, type SaveOutcome } from '../modules/configEditor';

const { t } = useI18n()

const props = defineProps<{
    api: Api.RemoteClient;
    newConfigGenerator?: () => NetworkTypes.NetworkConfig;
    pauseAutoRefresh?: boolean;
    scopeKey?: string;
}>();

const instanceId = defineModel('instanceId', {
    type: String as () => string | undefined,
    required: false,
})

const emits = defineEmits(['update']);

const toast = useToast();

/** Best-effort extraction of a readable message from a REST/RPC error. */
const formatError = (e: any): string => {
    const data = e?.response?.data ?? e;
    if (typeof data === 'string') {
        return data;
    }
    if (data && typeof data === 'object' && typeof data.message === 'string') {
        return data.message;
    }
    try {
        return JSON.stringify(data);
    } catch {
        return String(data);
    }
};
const showError = (detail: string) => toast.add({ severity: 'error', summary: t('web.common.error'), detail, life: 5000 });

const configFile = ref();

const curNetworkInfo = ref<NetworkTypes.NetworkInstance | null>(null);

const scope = computed(() => props.scopeKey ?? props.api.scope ?? '');
let generation = 0;
let instanceListRequest = 0;
const requestContext = () => {
    const api = props.api;
    const requestScope = scope.value;
    const requestGeneration = generation;
    return { api, current: () => api === props.api && requestScope === scope.value && requestGeneration === generation };
};

const listInstanceIdResponse = ref<Api.ListNetworkInstanceIdResponse | undefined>(undefined);
const runtimeCapabilities = computed(() => listInstanceIdResponse.value?.runtime_capabilities ?? []);
const revisionSupported = computed(() => supportsRevisionStore(props.api, runtimeCapabilities.value,
    listInstanceIdResponse.value?.support_local_config_revision));
const configStore = (): ConfigStore => createConfigStore(props.api, runtimeCapabilities.value, revisionSupported.value);

// One editing session of the selected network, or a new network not yet saved.
const editor = createConfigEditor();
const editing = ref(false);
const creating = ref(false);
const newNetworkConfig = ref<NetworkTypes.NetworkConfig>();
const showConfigEditDialog = ref(false);
const formConfig = computed(() => creating.value ? newNetworkConfig.value : editor.draft.value);
const lifecycleBusy = ref(false);
const busy = computed(() => editor.saving.value || lifecycleBusy.value);

const isRunning = (instanceId: string) => {
    return (listInstanceIdResponse.value?.running_inst_ids ?? []).map(Utils.UuidToStr).includes(instanceId);
}

const networkMetaCache = ref<Record<string, Api.NetworkMeta>>({});
const loadNetworkMetas = async (instanceIds: string[]) => {
    const missingIds = instanceIds.filter(id => !networkMetaCache.value[id]);

    if (missingIds.length === 0) return;

    try {
        const context = requestContext();
        const response = await context.api.get_network_metas(missingIds);
        if (!context.current()) return;
        Object.assign(networkMetaCache.value, response.metas ?? {});
    } catch (e) {
        console.error("Failed to load network metas", e);
    }
};
const reloadNetworkMeta = async (id: string) => {
    delete networkMetaCache.value[id];
    await loadNetworkMetas([id]);
};
const onLazyLoadNetworkMetas = async (event: VirtualScrollerLazyEvent) => {
    const instanceIds = instanceList.value
        .slice(event.first, event.last + 1)
        .map(item => item.uuid);
    await loadNetworkMetas(instanceIds);
};
const currentNetworkMeta = computed(() => {
    if (!instanceId.value) {
        return undefined;
    }
    return networkMetaCache.value[instanceId.value];
});
const currentNetworkControl = {
    remoteSave: computed(() => {
        return Api.ConfigFilePermission.isRemoveSaveable(currentNetworkMeta.value?.config_permission ?? 0);
    }),
    editable: computed(() => {
        return Api.ConfigFilePermission.isEditable(currentNetworkMeta.value?.config_permission ?? 0);
    }),
    deletable: computed(() => {
        return Api.ConfigFilePermission.isDeletable(currentNetworkMeta.value?.config_permission ?? 0);
    })
}

const instanceList = ref<Array<{ uuid: string; meta?: Api.NetworkMeta }>>([]);
const updateInstanceList = () => {
    const response = listInstanceIdResponse.value;
    const ids = [...new Set([...(response?.running_inst_ids ?? []), ...(response?.disabled_inst_ids ?? [])]
        .map(Utils.UuidToStr).filter(Boolean))].sort();
    const previous = new Map(instanceList.value.map(instance => [instance.uuid, instance]));
    const newList = ids.map(uuid => {
        const instance = previous.get(uuid) ?? { uuid, meta: undefined as Api.NetworkMeta | undefined };
        instance.meta = networkMetaCache.value[uuid];
        return instance;
    });

    // RPC catalog order and metadata refreshes must not replace Select options.
    if (newList.length !== instanceList.value.length || newList.some((instance, index) => instance !== instanceList.value[index])) {
        instanceList.value = newList;
    }
}
watch(listInstanceIdResponse, updateInstanceList, { deep: false });
watch(networkMetaCache, updateInstanceList, { deep: true });
watch(instanceList, async (newVal) => {
    if (newVal) {
        const instanceIds = new Set(newVal.map(item => item.uuid));
        Object.keys(networkMetaCache.value).forEach(id => {
            if (!instanceIds.has(id)) {
                delete networkMetaCache.value[id];
            }
        });
    }
});

const selectedInstanceId = computed({
    get() {
        return instanceList.value.find((instance) => instance.uuid === instanceId.value);
    },
    set(value: { uuid: string } | string | undefined) {
        // The web console binds this model directly to a route parameter,
        // while the desktop GUI binds it to a string ref. Keep the model
        // contract string-based so both consumers receive a valid instance
        // id instead of a Select option object.
        instanceId.value = typeof value === 'string' ? value : value?.uuid;
    }
});

const networkIsDisabled = computed(() => {
    if (!selectedInstanceId.value) {
        return false;
    }
    return (listInstanceIdResponse.value?.disabled_inst_ids ?? []).map(Utils.UuidToStr).includes(selectedInstanceId.value?.uuid);
});

// Running networks show their status until edited; stopped networks show their form.
const showForm = computed(() => creating.value || (!!selectedInstanceId.value && (editing.value || networkIsDisabled.value)));
const needShowNetworkStatus = computed(() => !!selectedInstanceId.value && !showForm.value);

const openEditor = async () => {
    const selected = selectedInstanceId.value?.uuid;
    if (!selected) return;
    const context = requestContext();
    try {
        await editor.open(configStore(), selected);
    } catch (error) {
        if (context.current() && selectedInstanceId.value?.uuid === selected) showError(formatError(error));
    }
};

const closeEditor = () => {
    editor.close();
    editing.value = false;
    showConfigEditDialog.value = false;
};

watch([() => selectedInstanceId.value?.uuid, networkIsDisabled], async ([selected, disabled], [previous, wasDisabled]) => {
    if (selected !== previous) {
        closeEditor();
        if (selected) creating.value = false;
    }
    // Stopping a network that is being edited keeps its draft.
    if (selected && disabled && (selected !== previous || !wasDisabled)
        && editor.base.value?.instanceId !== selected) await openEditor();
    // A network started elsewhere leaves its form unless it is being edited.
    if (selected && !disabled && wasDisabled && selected === previous && !editing.value) closeEditor();
    await loadCurrentNetworkInfo();
    if (selected && !networkMetaCache.value[selected]) await loadNetworkMetas([selected]);
});

const persist = () => currentNetworkControl.remoteSave.value;

// Applying runs the network with the edits: a running network restarts with
// them and a stopped one starts. Saving alone never starts a network.
const saveNetwork = async (start: boolean) => {
    const selected = editor.base.value?.instanceId;
    if (!selected || busy.value) return;
    const running = isRunning(selected);
    const context = requestContext();
    let outcome: SaveOutcome;
    try {
        outcome = await editor.save({ running, persist: persist() });
    } catch (error) {
        if (!context.current()) return;
        showError(error instanceof ConfigApplyError
            ? t('web.local_configs.apply_failed', { error: error.message })
            : t('web.device_management.save_config_failed', { error: formatError(error) }));
        return;
    }
    if (!context.current()) return;
    if (outcome === 'conflict') {
        toast.add({ severity: 'warn', summary: t('web.common.error'), detail: t('web.local_configs.changed_elsewhere'), life: 5000 });
        return;
    }
    const starting = start && !running;
    if (starting && !await setNetworkEnabled(true)) return;
    if (outcome !== 'saved') return;
    toast.add({ severity: 'success', summary: t('web.common.success'), detail: t(running || starting ? 'web.local_configs.saved_applied' : 'web.device_management.config_saved'), life: 2000 });
    if (running) closeEditor();
    await Promise.all([reloadNetworkMeta(selected), loadNetworkInstanceIds()]);
    await loadCurrentNetworkInfo();
};

const setNetworkEnabled = async (enabled: boolean): Promise<boolean> => {
    const selected = selectedInstanceId.value?.uuid;
    if (!selected || busy.value) return false;
    const context = requestContext();
    lifecycleBusy.value = true;
    try {
        await configStore().setEnabled(selected, enabled);
    } catch (e: any) {
        if (!context.current()) return false;
        console.error(e);
        showError(t(enabled ? 'web.local_configs.enable_failed' : 'web.device_management.disable_network_failed', { error: formatError(e) }));
        return false;
    } finally {
        if (context.current()) lifecycleBusy.value = false;
    }
    if (enabled) closeEditor();
    await loadNetworkInstanceIds();
    return true;
}

const confirm = useConfirm();
const confirmDeleteNetwork = (event: any) => {
    const selected = instanceId.value;
    if (!selected) return;
    const context = requestContext();
    confirm.require({
        target: event.currentTarget,
        message: 'Do you want to delete this network?',
        icon: 'pi pi-info-circle',
        rejectProps: {
            label: 'Cancel',
            severity: 'secondary',
            outlined: true
        },
        acceptProps: {
            label: 'Delete',
            severity: 'danger'
        },
        accept: async () => {
            try {
                if (!context.current() || instanceId.value !== selected) return;
                await configStore().remove(selected);
            } catch (e: any) {
                console.error(e);
                showError(t("web.device_management.delete_network_failed", { error: formatError(e) }));
                return;
            }
            emits('update');
        },
        reject: () => {
            return;
        }
    });
};

const newNetwork = () => {
    closeEditor();
    newNetworkConfig.value = props.newConfigGenerator?.() ?? NetworkTypes.DEFAULT_NETWORK_CONFIG();
    creating.value = true;
}

const createNetwork = async (start: boolean) => {
    const config = newNetworkConfig.value;
    if (!config || busy.value) return;
    const context = requestContext();
    lifecycleBusy.value = true;
    try {
        // Creation writes the network and optionally starts it; it never replaces an existing one.
        await configStore().create(NetworkTypes.normalizeNetworkConfig(config), start, true);
    } catch (e: any) {
        if (!context.current()) return;
        console.error(e);
        showError(t("web.device_management.create_network_failed", { error: formatError(e) }));
        return;
    } finally {
        if (context.current()) lifecycleBusy.value = false;
    }
    if (!context.current()) return;
    creating.value = false;
    newNetworkConfig.value = undefined;
    if (!start) toast.add({ severity: 'success', summary: t('web.common.success'), detail: t('web.device_management.config_saved'), life: 2000 });
    await loadNetworkInstanceIds();
    if (!context.current()) return;
    selectedInstanceId.value = config.instance_id;
    emits('update');
}

const cancelEdit = () => {
    if (creating.value) {
        creating.value = false;
        newNetworkConfig.value = undefined;
        return;
    }
    if (networkIsDisabled.value) editor.discard();
    else closeEditor();
}

const editNetwork = async () => {
    const selected = instanceId.value;
    if (!selected) {
        showError('No network instance selected');
        return;
    }
    await openEditor();
    if (editor.base.value?.instanceId === selected) editing.value = true;
}

const loadNetworkInstanceIds = async () => {
    const context = requestContext();
    const request = ++instanceListRequest;
    const response = await context.api.list_network_instance_ids();
    if (context.current() && request === instanceListRequest) listInstanceIdResponse.value = response;
}

const loadCurrentNetworkInfo = async () => {
    const selected = selectedInstanceId.value?.uuid;
    if (!selected) {
        curNetworkInfo.value = null;
        return;
    }
    if (!needShowNetworkStatus.value) {
        curNetworkInfo.value = null;
        return;
    }
    if (curNetworkInfo.value?.instance_id !== selected) {
        curNetworkInfo.value = null;
    }

    const context = requestContext();
    let network_info = await context.api.get_network_info(selected);
    if (!context.current() || selectedInstanceId.value?.uuid !== selected) {
        return;
    }

    if (!network_info) {
        curNetworkInfo.value = {
            instance_id: selected,
            running: false,
            error_msg: t('web.device_management.network_info_unavailable'),
        } as NetworkTypes.NetworkInstance;
        return;
    }

    curNetworkInfo.value = {
        instance_id: selected,
        running: network_info?.running ?? false,
        error_msg: network_info?.error_msg ?? '',
        detail: network_info,
    } as NetworkTypes.NetworkInstance;
}

const exportConfig = async () => {
    const selected = instanceId.value;
    if (!selected) {
        showError('No network instance selected');
        return;
    }

    try {
        const document = await configStore().load(selected);
        let toml = document.toml;
        if (toml === undefined) {
            const { instance_id, ...networkConfig } = document.config;
            const generated = await props.api.generate_config(networkConfig as NetworkTypes.NetworkConfig);
            if (generated.error) throw { response: { data: generated.error } };
            toml = generated.toml_config ?? '';
        }
        exportTomlFile(toml, selected + '.toml');
    } catch (e: any) {
        console.error(e);
        showError('Failed to export network config, error: ' + formatError(e));
    }
}

const importConfig = () => {
    configFile.value.click();
}

const replaceFormConfig = (config: NetworkTypes.NetworkConfig) => {
    const target = formConfig.value;
    if (!target) return;
    config.instance_id = target.instance_id;
    if (creating.value) newNetworkConfig.value = config;
    else editor.draft.value = config;
}

const handleFileUpload = (event: Event) => {
    const files = (event.target as HTMLInputElement).files;
    const file = files ? files[0] : null;
    if (!file) return;
    const reader = new FileReader();
    reader.onload = async (e) => {
        try {
            let tomlConfig = e.target?.result?.toString();
            if (!tomlConfig) return;
            const resp = await props.api.parse_config(tomlConfig);
            if (resp.error) {
                throw resp.error;
            }
            if (!resp.config) return;
            replaceFormConfig(resp.config);
            toast.add({ severity: 'success', summary: 'Import Success', detail: "Config file import success", life: 2000 });
        } catch (error) {
            toast.add({ severity: 'error', summary: 'Error', detail: 'Config file parse error: ' + error, life: 2000 });
        }
        configFile.value.value = null;
    }
    reader.readAsText(file);
}

const exportTomlFile = (context: string, name: string) => {
    let url = window.URL.createObjectURL(new Blob([context], { type: 'application/toml' }));
    let link = document.createElement('a');
    link.style.display = 'none';
    link.href = url;
    link.setAttribute('download', name);
    document.body.appendChild(link);
    link.click();

    document.body.removeChild(link);
    window.URL.revokeObjectURL(url);
}

// The file editor edits the node's original file when the node exposes it.
// Otherwise (official nodes, or a network being created) it edits TOML
// generated from the form and puts the result back into the form.
const editsOriginalFile = computed(() => !creating.value && editor.store.value?.editsOriginalFile === true);
const fileEditorDisabled = computed(() => !formConfig.value || busy.value || (editsOriginalFile.value && editor.dirty.value));

const openConfigFileEditor = () => {
    if (fileEditorDisabled.value || showConfigEditDialog.value) return;
    showConfigEditDialog.value = true;
};

const generateConfig = async (config: NetworkTypes.NetworkConfig): Promise<string> => {
    if (editsOriginalFile.value) return editor.base.value?.toml ?? '';
    let { toml_config: tomlConfig, error } = await props.api.generate_config(config);
    if (error) {
        throw error;
    }
    return tomlConfig ?? '';
}

const saveConfigFile = async (tomlConfig: string): Promise<void> => {
    if (editsOriginalFile.value) {
        const selected = editor.base.value!.instanceId;
        const running = isRunning(selected);
        let outcome: SaveOutcome;
        try {
            outcome = await editor.saveFile(tomlConfig, { running, persist: persist() });
        } catch (error) {
            if (error instanceof ConfigApplyError) throw new Error(t('web.local_configs.apply_failed', { error: error.message }));
            throw error;
        }
        if (outcome === 'conflict') throw new Error(t('web.local_configs.file_changed_elsewhere'));
        if (outcome === 'saved') {
            toast.add({ severity: 'success', summary: t('web.common.success'), detail: t(running ? 'web.local_configs.saved_applied' : 'web.device_management.config_saved'), life: 2000 });
            void reloadNetworkMeta(selected);
        }
        return;
    }
    let resp = await props.api.parse_config(tomlConfig);
    if (resp.error) {
        throw resp.error;
    };
    if (!resp.config) {
        throw new Error("Parsed config is empty");
    }
    replaceFormConfig(resp.config);
}

const formActionLabel = computed(() => creating.value ? t('web.local_configs.create_and_start') : t('web.local_configs.apply'));
const submitForm = () => creating.value ? createNetwork(true) : saveNetwork(true);
const saveWithoutStarting = () => creating.value ? createNetwork(false) : saveNetwork(false);

// 响应式屏幕宽度
const screenWidth = ref(window.innerWidth);
const updateScreenWidth = () => {
    screenWidth.value = window.innerWidth;
};

// 菜单引用和菜单项
const menuRef = ref();
const actionMenu: Ref<MenuItem[]> = ref([
    {
        label: () => t('web.device_management.edit_network'),
        icon: 'pi pi-pencil',
        visible: () => !(networkIsDisabled.value ?? true) && currentNetworkControl.editable.value,
        command: () => editNetwork()
    },
    {
        label: () => t('web.device_management.export_config'),
        icon: 'pi pi-download',
        command: () => exportConfig()
    },
    {
        label: () => t('web.device_management.delete_network'),
        icon: 'pi pi-trash',
        class: 'p-error',
        visible: () => currentNetworkControl.deletable.value,
        command: () => confirmDeleteNetwork(new Event('click'))
    }
]);

// Configurations change on the node (local edits, other consoles). Follow them
// while the form is open; overlapping edits are surfaced instead of replaced.
let tick = 0;
let periodFunc = new Utils.PeriodicTask(async () => {
    if (props.pauseAutoRefresh) {
        return;
    }
    try {
        await Promise.all([loadNetworkInstanceIds(), loadCurrentNetworkInfo()]);
        if (tick++ % 2 === 0 && editor.base.value && !showConfigEditDialog.value) await editor.refresh();
    } catch (e) {
        console.debug(e);
    }
}, 1000);

watch([() => props.api, scope], () => {
    generation++;
    closeEditor();
    creating.value = false;
    newNetworkConfig.value = undefined;
    lifecycleBusy.value = false;
    curNetworkInfo.value = null;
    listInstanceIdResponse.value = undefined;
    networkMetaCache.value = {};
    instanceList.value = [];
    void loadNetworkInstanceIds().catch(console.debug);
});

onMounted(async () => {
    periodFunc.start();

    // 添加屏幕尺寸监听
    window.addEventListener('resize', updateScreenWidth);
});

onUnmounted(() => {
    generation++;
    editor.close();
    periodFunc.stop();

    // 移除屏幕尺寸监听
    window.removeEventListener('resize', updateScreenWidth);
});

const editorDirty = editor.dirty;
const editorRemote = editor.remote;
const editorConflicts = editor.conflictFields;
const editorMerged = editor.merged;
</script>


<template>
    <div class="device-management">
        <input type="file" @change="handleFileUpload" class="hidden" accept="application/toml" ref="configFile" />
        <ConfirmPopup></ConfirmPopup>

        <!-- 网络选择和操作按钮始终在同一行 -->
        <div class="network-header mb-3">
            <div class="flex flex-row justify-between items-center gap-2" style="align-items: center;">
                <!-- 网络选择 -->
                <div class="flex-1 min-w-0">
                    <IftaLabel class="w-full">
                        <Select v-model="selectedInstanceId" :options="instanceList" optionLabel="uuid" dataKey="uuid" class="w-full"
                            inputId="dd-inst-id" :placeholder="t('web.device_management.select_network')"
                            :pt="{ root: { class: 'network-select-container' } }" :virtualScrollerOptions="{
                                lazy: true,
                                onLazyLoad: onLazyLoadNetworkMetas,
                                itemSize: 60,
                                delay: 50
                            }">
                            <template #value="slotProps">
                                <div v-if="slotProps.value" class="flex items-center content-center min-w-0">
                                    <div class="mr-4 flex-col min-w-0 flex-1">
                                        <span class="truncate block">
                                            &nbsp;
                                            <span v-if="slotProps.value.meta">
                                                {{ slotProps.value.meta.network_name }} ({{ slotProps.value.uuid }})
                                            </span>
                                            <span v-else>
                                                {{ slotProps.value.uuid }}
                                            </span>
                                        </span>
                                    </div>
                                    <Tag class="my-auto leading-3 shrink-0"
                                        :severity="isRunning(slotProps.value.uuid) ? 'success' : 'info'"
                                        :value="t(isRunning(slotProps.value.uuid) ? 'network_running' : 'network_stopped')" />
                                </div>
                                <span v-else>
                                    {{ slotProps.placeholder }}
                                </span>
                            </template>
                            <template #option="slotProps">
                                <div class="flex flex-col items-start content-center max-w-full">
                                    <div class="flex items-center min-w-0">
                                        <div class="mr-4 min-w-0 flex-1">
                                            <span class="truncate block">{{ t('network_name') }}: {{
                                                slotProps.option.meta?.network_name ?? slotProps.option.uuid }}</span>
                                        </div>
                                        <Tag class="my-auto leading-3 shrink-0"
                                            :severity="isRunning(slotProps.option.uuid) ? 'success' : 'info'"
                                            :value="t(isRunning(slotProps.option.uuid) ? 'network_running' : 'network_stopped')" />
                                    </div>
                                    <div class="max-w-full overflow-hidden text-ellipsis text-gray-500">
                                        {{ slotProps.option.uuid }}
                                    </div>
                                </div>
                            </template>
                        </Select>
                        <label class="network-label mr-2 font-medium" for="dd-inst-id">{{
                            t('web.device_management.network') }}</label>
                    </IftaLabel>
                </div>

                <!-- 简化的按钮区域 - 无论屏幕大小都显示 -->
                <div class="flex gap-2 shrink-0 button-container items-center">
                    <!-- Create/Cancel button based on state -->
                    <Button v-if="!creating && !editing" @click="newNetwork" icon="pi pi-plus" :disabled="busy"
                        :label="screenWidth > 640 ? t('web.device_management.create_new') : undefined"
                        :class="['create-button', screenWidth <= 640 ? 'p-button-icon-only' : '']"
                        :style="screenWidth <= 640 ? 'width: 3rem !important; height: 3rem !important; font-size: 1.2rem' : ''"
                        :tooltip="screenWidth <= 640 ? t('web.device_management.create_network') : undefined"
                        tooltipOptions="{ position: 'bottom' }" severity="primary" />

                    <Button v-else @click="cancelEdit" icon="pi pi-times" :disabled="busy"
                        :label="screenWidth > 640 ? t('web.device_management.cancel_edit') : undefined"
                        :class="['cancel-button', screenWidth <= 640 ? 'p-button-icon-only' : '']"
                        :style="screenWidth <= 640 ? 'width: 3rem !important; height: 3rem !important; font-size: 1.2rem' : ''"
                        :tooltip="screenWidth <= 640 ? t('web.device_management.cancel_edit') : undefined"
                        tooltipOptions="{ position: 'bottom' }" severity="secondary" />

                    <!-- More actions menu -->
                    <Menu ref="menuRef" :model="actionMenu" :popup="true" />
                    <Button v-if="!creating && !editing && selectedInstanceId" icon="pi pi-ellipsis-v"
                        class="p-button-rounded flex items-center justify-center" severity="help"
                        style="width: 3rem !important; height: 3rem !important; font-size: 1.2rem"
                        @click="menuRef.toggle($event)" :aria-label="t('web.device_management.more_actions')"
                        :tooltip="t('web.device_management.more_actions')" tooltipOptions="{ position: 'bottom' }" />
                </div>
            </div>
        </div>

        <!-- Main Content Area -->
        <div class="network-content">
            <!-- Network Form -->
            <div v-if="showForm" class="network-creation-container">
                <div class="network-creation-header flex items-center gap-2 mb-3">
                    <i class="pi pi-plus-circle text-primary text-xl"></i>
                    <h2 class="text-xl font-medium">{{ t(creating ? 'web.device_management.create_network' : 'web.device_management.edit_network') }}</h2>
                </div>

                <div class="w-full flex gap-2 flex-wrap justify-start mb-3">
                    <Button @click="openConfigFileEditor" icon="pi pi-file-edit"
                        :label="t('web.device_management.edit_as_file')" iconPos="left" severity="secondary"
                        :disabled="fileEditorDisabled" />
                    <Button @click="importConfig" icon="pi pi-upload" :label="t('web.device_management.import_config')"
                        iconPos="left" severity="help" :disabled="busy || !formConfig" />
                    <Button v-if="creating || networkIsDisabled" @click="saveWithoutStarting" icon="pi pi-save"
                        :label="t('web.device_management.save_config')" severity="secondary"
                        :disabled="busy || !formConfig || !!editorRemote || (!creating && !editorDirty)" />
                    <Button v-if="!creating && networkIsDisabled && editorDirty" @click="cancelEdit" icon="pi pi-undo"
                        :label="t('web.local_configs.discard')" severity="secondary" :disabled="busy" />
                    <Button v-if="!creating && networkIsDisabled" @click="setNetworkEnabled(true)" icon="pi pi-play"
                        :label="t('web.local_configs.enable')" severity="success"
                        :disabled="busy || editorDirty" />
                </div>

                <Divider />

                <Message v-if="editsOriginalFile && editorDirty" severity="secondary" :closable="false" class="mb-3">
                    {{ t('web.local_configs.file_editor_needs_save') }}</Message>
                <Message v-if="editorMerged && !editorRemote" severity="info" :closable="false" class="mb-3">
                    {{ t('web.local_configs.merged_elsewhere') }}</Message>
                <Message v-if="editorRemote" severity="warn" :closable="false" class="mb-3">
                    <div>{{ t('web.local_configs.conflict_fields', { fields: editorConflicts.join(', ') }) }}</div>
                    <div class="flex gap-2 mt-2">
                        <Button size="small" :label="t('web.local_configs.keep_mine')" @click="editor.keepMine()" />
                        <Button size="small" severity="secondary" :label="t('web.local_configs.take_theirs')" @click="editor.takeTheirs()" />
                    </div>
                </Message>

                <Config :cur-network="formConfig" :config-invalid="!formConfig || busy || !!editorRemote"
                    :runtime-capabilities="runtimeCapabilities" :action-label="formActionLabel"
                    @run-network="submitForm"></Config>
            </div>

            <!-- Network Status (for running networks) -->
            <div v-else-if="needShowNetworkStatus" class="network-status-container">
                <div class="network-status-header flex items-center gap-2 mb-3">
                    <i class="pi pi-chart-line text-primary text-xl"></i>
                    <h2 class="text-xl font-medium">{{ t('web.device_management.network_status') }}</h2>
                </div>

                <Status v-if="curNetworkInfo && curNetworkInfo.error_msg === ''" v-bind:cur-network-inst="curNetworkInfo"
                    :api="api" :readonly="!currentNetworkControl.editable.value"
                    class="mb-4">
                </Status>
                <Message v-else-if="curNetworkInfo?.error_msg" severity="error" class="mb-4">{{
                    curNetworkInfo.error_msg }}</Message>
                <Message v-else severity="info" class="mb-4">{{ t('web.device_management.loading_network_status') }}
                </Message>

                <div class="text-center mt-4">
                    <Button @click="setNetworkEnabled(false)" :disabled="busy || !currentNetworkControl.deletable.value"
                        :label="t('web.device_management.disable_network')" severity="danger" icon="pi pi-power-off"
                        iconPos="left" />
                </div>
            </div>

            <!-- Empty State -->
            <div v-else class="empty-state flex flex-col items-center py-12">
                <i class="pi pi-sitemap text-5xl text-secondary mb-4 opacity-50"></i>
                <div class="text-xl text-center font-medium mb-3">{{ t('web.device_management.no_network_selected') }}
                </div>
                <p class="text-secondary text-center mb-6 max-w-md">
                    {{ t('web.device_management.select_existing_network_or_create_new') }}
                </p>
                <Button @click="newNetwork" :label="t('web.device_management.create_network')" icon="pi pi-plus"
                    iconPos="left" />
            </div>
        </div>

        <ConfigEditDialog v-model:visible="showConfigEditDialog" :cur-network="formConfig"
            :generate-config="generateConfig" :save-config="saveConfigFile" />
    </div>
</template>

<style scoped>
.device-management {
    height: 100%;
    display: flex;
    flex-direction: column;
    /* hosts that mount this full-page (easytier-gui) have no padding of
       their own, so keep a safe inset here; panel chrome stays with hosts */
    padding: 0.75rem;
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

/* Dark mode adaptations: surface utilities follow host theme tokens; the
   app-dark branch supplies dark fallbacks for hosts that define no tokens.
   :global must own the whole selector — mixing it with :deep in one selector
   makes the compiler drop the :deep part. */
:deep(.bg-surface-50) {
    background-color: var(--surface-ground, #f8fafc);
}

:deep(.bg-surface-0) {
    background-color: var(--surface-card, #ffffff);
}

:deep(.text-primary) {
    color: var(--primary-color, #3b82f6);
}

:deep(.text-secondary) {
    color: var(--text-color-secondary, #64748b);
}

:global(html.app-dark .device-management .bg-surface-50) {
    background-color: var(--surface-ground, #0f172a);
}

:global(html.app-dark .device-management .bg-surface-0) {
    background-color: var(--surface-card, #1e293b);
}

/* Responsive design for mobile devices */
@media (max-width: 768px) {

    /* 在小屏幕上缩短网络标签文本 */
    .network-label {
        font-size: 0.9rem;
    }
}
</style>
