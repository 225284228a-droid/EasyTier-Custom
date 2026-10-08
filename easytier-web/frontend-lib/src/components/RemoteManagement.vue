<script setup lang="ts">
import { Button, ConfirmPopup, Divider, IftaLabel, Menu, Message, Select, Tag, useConfirm, useToast, type VirtualScrollerLazyEvent } from 'primevue';
import { computed, onMounted, onUnmounted, Ref, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import * as Api from '../modules/api';
import * as Utils from '../modules/utils';
import * as NetworkTypes from '../types/network';
import { type MenuItem } from 'primevue/menuitem';
import { LOCAL_CONFIG_APPLY_CAPABILITY, LOCAL_CONFIG_REVISION_CAPABILITY } from '../modules/capabilities';
import {
    buildLocalConfigApplyRequest, buildLocalConfigPatch, buildLocalConfigCreatePatch, changedConfigFields, cloneEditableConfig, localConfigEditable,
    localConfigInstanceId, LocalConfigApplyMode, patchStatusName,
    type LocalConfigEntry, type LocalConfigPatchResult, type LocalConfigSnapshot,
} from '../modules/localConfigPatch';

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

const configFile = ref();

const curNetworkInfo = ref<NetworkTypes.NetworkInstance | null>(null);

const showConfigEditDialog = ref(false);
const isEditingNetwork = ref(false); // Flag to indicate if we're in network editing mode
const currentNetworkConfig = ref<NetworkTypes.NetworkConfig | undefined>(undefined);
const configBaseline = ref<NetworkTypes.NetworkConfig>();
const editingEntry = ref<LocalConfigEntry>();
const configSubmitting = ref(false);
const scope = computed(() => props.scopeKey ?? props.api.scope ?? '');
let generation = 0;
let configRequest = 0;
const dirtyFields = computed(() => currentNetworkConfig.value && configBaseline.value
    ? changedConfigFields(configBaseline.value, currentNetworkConfig.value) : []);
const dirty = computed(() => dirtyFields.value.length > 0);
const clearConfig = () => {
    configRequest++;
    currentNetworkConfig.value = undefined;
    configBaseline.value = undefined;
    editingEntry.value = undefined;
};
const requestContext = () => {
    const api = props.api;
    const requestScope = scope.value;
    const requestGeneration = generation;
    return { api, current: () => api === props.api && requestScope === scope.value && requestGeneration === generation };
};

const listInstanceIdResponse = ref<Api.ListNetworkInstanceIdResponse | undefined>(undefined);
const runtimeCapabilities = computed(() => listInstanceIdResponse.value?.runtime_capabilities ?? []);
const revisionSupported = computed(() => !!props.api.observe_local_configs && !!props.api.patch_local_config
    && (listInstanceIdResponse.value?.support_local_config_revision === true
        || runtimeCapabilities.value.includes(LOCAL_CONFIG_REVISION_CAPABILITY)));
const applySupported = computed(() => runtimeCapabilities.value.includes(LOCAL_CONFIG_APPLY_CAPABILITY));
const applySavedConfig = computed(() => !!editingEntry.value?.running && editingEntry.value.pending_apply && !dirty.value);
const configActionLabel = computed(() => applySavedConfig.value ? t('web.local_configs.apply_saved')
    : editingEntry.value?.running ? t('web.local_configs.save_apply') : undefined);

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
    let insts = new Set<string>();
    let t = listInstanceIdResponse.value;
    if (t) {
        (t.running_inst_ids ?? []).forEach((u) => insts.add(Utils.UuidToStr(u)));
        (t.disabled_inst_ids ?? []).forEach((u) => insts.add(Utils.UuidToStr(u)));
    }

    const newList = Array.from(insts).map((instance: string) => {
        return {
            uuid: instance,
            meta: networkMetaCache.value[instance]
        };
    });

    if (JSON.stringify(newList) !== JSON.stringify(instanceList.value)) {
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
watch(selectedInstanceId, async (newVal, oldVal) => {
    if (newVal?.uuid !== oldVal?.uuid) {
        clearConfig();
        isEditingNetwork.value = false;
        showConfigEditDialog.value = false;
        if (networkIsDisabled.value) {
            try { await loadCurrentNetworkConfig(); }
            catch (error) { toast.add({ severity: 'warn', summary: t('web.common.error'), detail: formatError(error), life: 5000 }); }
        }
    }
    await loadCurrentNetworkInfo();

    if (newVal?.uuid && !networkMetaCache.value[newVal.uuid]) {
        await loadNetworkMetas([newVal.uuid]);
    }
});

const needShowNetworkStatus = computed(() => {
    if (!selectedInstanceId.value) {
        // nothing selected
        return false;
    }
    if (networkIsDisabled.value) {
        // network is disabled
        return false;
    }
    if (isEditingNetwork.value || dirty.value) {
        // editing network
        return false;
    }
    return true;
})

const networkIsDisabled = computed(() => {
    if (!selectedInstanceId.value) {
        return false;
    }
    return (listInstanceIdResponse.value?.disabled_inst_ids ?? []).map(Utils.UuidToStr).includes(selectedInstanceId.value?.uuid);
});
watch(networkIsDisabled, async (newVal, oldVal) => {
    if (newVal !== oldVal && newVal === true) {
        try { await loadCurrentNetworkConfig(); }
        catch (error) { toast.add({ severity: 'warn', summary: t('web.common.error'), detail: formatError(error), life: 5000 }); }
    }
});

const loadCurrentNetworkConfig = async (force = false) => {
    if (dirty.value && !force) return;
    const selected = selectedInstanceId.value?.uuid;
    if (!selected) { clearConfig(); return; }
    const context = requestContext();
    const request = ++configRequest;
    let config: NetworkTypes.NetworkConfig;
    let entry: LocalConfigEntry | undefined;
    if (revisionSupported.value) {
        const snapshot = await context.api.observe_local_configs!();
        entry = snapshot.entries.find(entry => localConfigInstanceId(entry) === selected);
        if (!entry || !localConfigEditable(snapshot, entry)) throw new Error(t('web.local_configs.unavailable'));
        config = entry.config!;
    } else {
        config = await context.api.get_network_config(selected);
    }
    if (!context.current() || request !== configRequest || selectedInstanceId.value?.uuid !== selected) return;
    currentNetworkConfig.value = cloneEditableConfig(config);
    configBaseline.value = cloneEditableConfig(config);
    editingEntry.value = entry ? { ...entry } : undefined;
}

const lifecycleRevision = async (api: Api.RemoteClient, selected: string): Promise<string | undefined> => {
    if (!revisionSupported.value) return undefined;
    const snapshot = await api.observe_local_configs!();
    const entry = snapshot.entries.find(entry => localConfigInstanceId(entry) === selected);
    if (!entry || !localConfigEditable(snapshot, entry)) throw new Error(t('web.local_configs.unavailable'));
    return entry.revision;
};

const saveEditedConfig = async (config: NetworkTypes.NetworkConfig, mode: LocalConfigApplyMode): Promise<LocalConfigEntry | undefined> => {
    if (!editingEntry.value) {
        if (revisionSupported.value) throw new Error(t('web.local_configs.reread'));
        return undefined;
    }
    if (!configBaseline.value) throw new Error(t('web.local_configs.reread'));
    const fields = changedConfigFields(configBaseline.value, config);
    const applyOnly = !fields.length && mode === LocalConfigApplyMode.SaveAndApply
        && editingEntry.value.running && editingEntry.value.pending_apply;
    if (!fields.length && !applyOnly) return editingEntry.value;
    if (applyOnly && !applySupported.value) throw new Error(t('web.local_configs.apply_unsupported'));
    const context = requestContext();
    const selected = localConfigInstanceId(editingEntry.value);
    const request = applyOnly ? buildLocalConfigApplyRequest(editingEntry.value, runtimeCapabilities.value)
        : buildLocalConfigPatch(editingEntry.value, config, fields, runtimeCapabilities.value, mode);
    const response = await context.api.patch_local_config!(request);
    if (!context.current() || instanceId.value !== selected) throw new Error('Management connection changed');
    const result = response as LocalConfigPatchResult;
    const status = patchStatusName(result.status);
    if (status !== 'success') throw new Error(result.message || t(`web.local_configs.${status}`));
    const snapshot = 'entries' in response ? response as LocalConfigSnapshot : result.snapshot;
    const entry = result.entry ?? snapshot?.entries.find(entry => localConfigInstanceId(entry) === selected);
    if (!entry?.revision) throw new Error(t('web.local_configs.reread'));
    editingEntry.value = { ...entry };
    configBaseline.value = cloneEditableConfig(config);
    return entry;
};

const rereadEditingRevision = async () => {
    const selected = instanceId.value;
    if (!selected || !revisionSupported.value) return;
    const context = requestContext();
    try {
        const snapshot = await context.api.observe_local_configs!();
        if (!context.current() || instanceId.value !== selected) return;
        const entry = snapshot.entries.find(entry => localConfigInstanceId(entry) === selected);
        if (!entry || !localConfigEditable(snapshot, entry)) throw new Error(t('web.local_configs.unavailable'));
        editingEntry.value = { ...entry };
        if (!dirty.value) {
            currentNetworkConfig.value = cloneEditableConfig(entry.config!);
            configBaseline.value = cloneEditableConfig(entry.config!);
        }
    } catch (error) {
        if (context.current()) toast.add({ severity: 'warn', summary: t('web.common.error'), detail: formatError(error), life: 5000 });
    }
};

const stopNetwork = async () => {
    if (!selectedInstanceId.value) {
        return;
    }

    try {
        const context = requestContext();
        const selected = selectedInstanceId.value.uuid;
        const revision = await lifecycleRevision(context.api, selected);
        if (!context.current() || selectedInstanceId.value?.uuid !== selected) return;
        if (revision) await context.api.update_network_instance_state(selected, true, revision);
        else await context.api.update_network_instance_state(selected, true);
    } catch (e: any) {
        console.error(e);
        toast.add({ severity: 'error', summary: t("web.common.error"), detail: t("web.device_management.disable_network_failed", { error: formatError(e) }), life: 5000 });
        return;
    }
    await loadNetworkInstanceIds();
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
                const revision = await lifecycleRevision(context.api, selected);
                if (!context.current() || instanceId.value !== selected) return;
                if (revision) await context.api.delete_network(selected, revision);
                else await context.api.delete_network(selected);
            } catch (e: any) {
                console.error(e);
                toast.add({ severity: 'error', summary: t("web.common.error"), detail: t("web.device_management.delete_network_failed", { error: formatError(e) }), life: 5000 });
                return;
            }
            emits('update');
        },
        reject: () => {
            return;
        }
    });
};

const saveAndRunNewNetwork = async (config?: NetworkTypes.NetworkConfig) => {
    if (configSubmitting.value) return;
    const editedConfig = config ?? currentNetworkConfig.value;
    if (!editedConfig) {
        return;
    }
    const cfg = cloneEditableConfig(editedConfig);

    const targetInstanceId = instanceId.value ?? cfg.instance_id;
    if (targetInstanceId && cfg.instance_id !== targetInstanceId) {
        cfg.instance_id = targetInstanceId;
    }

    const context = requestContext();
    const selected = instanceId.value;
    const request = configRequest;
    const current = () => context.current() && instanceId.value === selected && configRequest === request;
    configSubmitting.value = true;
    try {
        const disabled = networkIsDisabled.value;
        if (revisionSupported.value) {
            const entry = await saveEditedConfig(cfg, LocalConfigApplyMode.SaveAndApply);
            if (!current()) return;
            if (disabled) await context.api.update_network_instance_state(cfg.instance_id, false, entry!.revision);
        } else if (disabled) {
            await context.api.save_config(cfg);
            if (!current()) return;
            await context.api.update_network_instance_state(cfg.instance_id, false);
        } else {
            await context.api.run_network(cfg, currentNetworkControl.remoteSave.value);
        }
        if (!current()) return;

        // Acknowledge only the submitted snapshot; edits made during the request stay dirty.
        configBaseline.value = cloneEditableConfig(cfg);
        isEditingNetwork.value = false;

        delete networkMetaCache.value[cfg.instance_id];
        await loadNetworkMetas([cfg.instance_id]);
        if (!current()) return;

        await loadNetworkInstanceIds();
        if (!current()) return;
        await loadCurrentNetworkInfo();
    } catch (e: any) {
        if (!current()) return;
        console.error(e);
        toast.add({ severity: 'error', summary: 'Error', detail: 'Failed to run network, error: ' + formatError(e), life: 2000 });
        return;
    } finally {
        if (context.current()) configSubmitting.value = false;
    }

    if (!current()) return;
    selectedInstanceId.value = cfg.instance_id;
    emits('update');
}

const saveNetworkConfig = async () => {
    if (!currentNetworkConfig.value || configSubmitting.value) {
        return;
    }
    const config = currentNetworkConfig.value;
    const context = requestContext();
    configSubmitting.value = true;
    try {
        if (revisionSupported.value) await saveEditedConfig(config, LocalConfigApplyMode.PersistOnly);
        else await context.api.save_config(config);
        if (!context.current()) return;

        delete networkMetaCache.value[config.instance_id];
        await loadNetworkMetas([config.instance_id]);
        if (context.current()) toast.add({ severity: 'success', summary: t("web.common.success"), detail: t("web.device_management.config_saved"), life: 2000 });
    } catch (e: any) {
        if (!context.current()) return;
        console.error(e);
        toast.add({ severity: 'error', summary: t("web.common.error"), detail: t("web.device_management.save_config_failed", { error: formatError(e) }), life: 5000 });
        return;
    } finally {
        if (context.current()) configSubmitting.value = false;
    }
}
const newNetwork = async () => {
    const newNetworkConfig = props.newConfigGenerator?.() ?? NetworkTypes.DEFAULT_NETWORK_CONFIG();
    // Surface failures instead of leaving the click without any feedback: the
    // console may reject the new config (e.g. read-only config dir or a name
    // collision with a running instance).
    try {
        const context = requestContext();
        if (revisionSupported.value) {
            const response = await context.api.patch_local_config!(buildLocalConfigCreatePatch(newNetworkConfig, runtimeCapabilities.value));
            const result = response as LocalConfigPatchResult;
            if (patchStatusName(result.status) !== 'success') throw new Error(result.message || t(`web.local_configs.${patchStatusName(result.status)}`));
        } else await context.api.save_config(newNetworkConfig);
        if (!context.current()) return;
    } catch (e: any) {
        console.error(e);
        toast.add({ severity: 'error', summary: t("web.common.error"), detail: t("web.device_management.create_network_failed", { error: formatError(e) }), life: 5000 });
        return;
    }
    selectedInstanceId.value = newNetworkConfig.instance_id;
    currentNetworkConfig.value = newNetworkConfig;
    delete networkMetaCache.value[newNetworkConfig.instance_id];
    await Promise.all([
        loadNetworkMetas([newNetworkConfig.instance_id]),
        loadNetworkInstanceIds(),
    ]);
    await loadCurrentNetworkConfig(true);
}

const cancelEditNetwork = () => {
    isEditingNetwork.value = false;
    clearConfig();
}

const editNetwork = async () => {
    if (!instanceId.value) {
        toast.add({ severity: 'error', summary: 'Error', detail: 'No network instance selected', life: 2000 });
        return;
    }

    try {
        await loadCurrentNetworkConfig(true);
        isEditingNetwork.value = true; // Switch to editing mode instead
    } catch (e: any) {
        console.error(e);
        toast.add({ severity: 'error', summary: t('web.common.error'), detail: formatError(e), life: 5000 });
        return;
    }
}

const loadNetworkInstanceIds = async () => {
    const context = requestContext();
    const response = await context.api.list_network_instance_ids();
    if (context.current()) listInstanceIdResponse.value = response;
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
    if (!instanceId.value) {
        toast.add({ severity: 'error', summary: 'Error', detail: 'No network instance selected', life: 2000 });
        return;
    }

    try {
        const { instance_id, ...networkConfig } = await props.api.get_network_config(instanceId.value!);
        let { toml_config: tomlConfig, error } = await props.api.generate_config(networkConfig as NetworkTypes.NetworkConfig);
        if (error) {
            throw { response: { data: error } };
        }
        exportTomlFile(tomlConfig ?? '', instanceId.value + '.toml');
    } catch (e: any) {
        console.error(e);
        toast.add({ severity: 'error', summary: 'Error', detail: 'Failed to export network config, error: ' + JSON.stringify(e.response.data), life: 2000 });
        return;
    }
}

const importConfig = () => {
    configFile.value.click();
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

            const config = resp.config;
            if (!config) return;

            config.instance_id = currentNetworkConfig.value?.instance_id ?? config?.instance_id;
            currentNetworkConfig.value = config;
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

const generateConfig = async (config: NetworkTypes.NetworkConfig): Promise<string> => {
    let { toml_config: tomlConfig, error } = await props.api.generate_config(config);
    if (error) {
        throw error;
    }
    return tomlConfig ?? '';
}

const syncTomlConfig = async (tomlConfig: string): Promise<void> => {
    let resp = await props.api.parse_config(tomlConfig);
    if (resp.error) {
        throw resp.error;
    };
    const config = resp.config;
    if (!config) {
        throw new Error("Parsed config is empty");
    }
    config.instance_id = currentNetworkConfig.value?.instance_id ?? config?.instance_id;
    currentNetworkConfig.value = config;
}

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

let periodFunc = new Utils.PeriodicTask(async () => {
    if (props.pauseAutoRefresh) {
        return;
    }
    try {
        await Promise.all([loadNetworkInstanceIds(), loadCurrentNetworkInfo()]);
    } catch (e) {
        console.debug(e);
    }
}, 1000);

watch([() => props.api, scope], () => {
    generation++;
    configSubmitting.value = false;
    clearConfig();
    isEditingNetwork.value = false;
    showConfigEditDialog.value = false;
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
    clearConfig();
    periodFunc.stop();

    // 移除屏幕尺寸监听
    window.removeEventListener('resize', updateScreenWidth);
});

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
                        <Select v-model="selectedInstanceId" :options="instanceList" optionLabel="uuid" class="w-full"
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
                    <Button v-if="!isEditingNetwork" @click="newNetwork" icon="pi pi-plus" :disabled="configSubmitting"
                        :label="screenWidth > 640 ? t('web.device_management.create_new') : undefined"
                        :class="['create-button', screenWidth <= 640 ? 'p-button-icon-only' : '']"
                        :style="screenWidth <= 640 ? 'width: 3rem !important; height: 3rem !important; font-size: 1.2rem' : ''"
                        :tooltip="screenWidth <= 640 ? t('web.device_management.create_network') : undefined"
                        tooltipOptions="{ position: 'bottom' }" severity="primary" />

                    <Button v-else @click="cancelEditNetwork" icon="pi pi-times" :disabled="configSubmitting"
                        :label="screenWidth > 640 ? t('web.device_management.cancel_edit') : undefined"
                        :class="['cancel-button', screenWidth <= 640 ? 'p-button-icon-only' : '']"
                        :style="screenWidth <= 640 ? 'width: 3rem !important; height: 3rem !important; font-size: 1.2rem' : ''"
                        :tooltip="screenWidth <= 640 ? t('web.device_management.cancel_edit') : undefined"
                        tooltipOptions="{ position: 'bottom' }" severity="secondary" />

                    <!-- More actions menu -->
                    <Menu ref="menuRef" :model="actionMenu" :popup="true" />
                    <Button v-if="!isEditingNetwork && selectedInstanceId" icon="pi pi-ellipsis-v"
                        class="p-button-rounded flex items-center justify-center" severity="help"
                        style="width: 3rem !important; height: 3rem !important; font-size: 1.2rem"
                        @click="menuRef.toggle($event)" :aria-label="t('web.device_management.more_actions')"
                        :tooltip="t('web.device_management.more_actions')" tooltipOptions="{ position: 'bottom' }" />
                </div>
            </div>
        </div>

        <!-- Main Content Area -->
        <div class="network-content">
            <!-- Network Creation Form -->
            <div v-if="isEditingNetwork || networkIsDisabled || dirty" class="network-creation-container">
                <div class="network-creation-header flex items-center gap-2 mb-3">
                    <i class="pi pi-plus-circle text-primary text-xl"></i>
                    <h2 class="text-xl font-medium">{{ t('web.device_management.edit_network') }}</h2>
                </div>

                <div class="w-full flex gap-2 flex-wrap justify-start mb-3">
                    <Button v-if="editingEntry" @click="rereadEditingRevision" icon="pi pi-refresh"
                        :label="t('web.local_configs.reread')" severity="secondary" outlined :disabled="configSubmitting" />
                    <Button @click="showConfigEditDialog = true" icon="pi pi-file-edit"
                        :label="t('web.device_management.edit_as_file')" iconPos="left" severity="secondary" :disabled="configSubmitting" />
                    <Button @click="importConfig" icon="pi pi-upload" :label="t('web.device_management.import_config')"
                        iconPos="left" severity="help" :disabled="configSubmitting" />
                    <Button @click="saveNetworkConfig" :disabled="!currentNetworkConfig || configSubmitting"
                        icon="pi pi-save" :label="t('web.device_management.save_config')" iconPos="left"
                        severity="success" />
                </div>

                <Divider />

                <Message v-if="dirty" severity="info" :closable="false" class="mb-3">{{ t('web.local_configs.dirty_preserved') }}</Message>
                <Message v-if="editingEntry?.pending_apply" severity="warn" :closable="false" class="mb-3">
                    {{ t('web.local_configs.pending_apply') }}
                    <span v-if="applySavedConfig && !applySupported"> — {{ t('web.local_configs.apply_unsupported') }}</span>
                </Message>

                <Config :cur-network="currentNetworkConfig" :config-invalid="!currentNetworkConfig || configSubmitting"
                    :runtime-capabilities="runtimeCapabilities" :action-label="configActionLabel"
                    @run-network="saveAndRunNewNetwork"></Config>
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
                    <Button @click="stopNetwork" :disabled="!currentNetworkControl.deletable.value"
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

        <!-- Keep only the config edit dialogs -->
        <!-- <ConfigEditDialog v-if="networkIsDisabled" v-model:visible="showCreateNetworkDialog"
            :cur-network="currentNetworkConfig" :generate-config="generateConfig" :save-config="saveConfig" /> -->

        <ConfigEditDialog v-model:visible="showConfigEditDialog" :cur-network="currentNetworkConfig"
            :generate-config="generateConfig" :save-config="syncTomlConfig" />
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
