<script setup lang="ts">

import { type } from '@tauri-apps/plugin-os'

import { invoke } from '@tauri-apps/api/core'
import { writeText } from '@tauri-apps/plugin-clipboard-manager'
import { open } from '@tauri-apps/plugin-shell'
import { exit } from '@tauri-apps/plugin-process'
import { I18nUtils, RemoteManagement, Utils } from "easytier-frontend-lib"
import type { MenuItem } from 'primevue/menuitem'
import { useTray } from '~/composables/tray'
import {
  consumePendingMobileVpnTileAction,
  initMobileVpnService,
  setMobileVpnTileActionHandler,
  syncMobileVpnService,
} from '~/composables/mobile_vpn'
import { executeVpnTileAction } from '~/composables/mobile_vpn_tile'
import { GUIRemoteClient } from '~/modules/api'

import { useToast, useConfirm } from 'primevue'
import { loadMode, saveMode, WebClientConfig, type Mode } from '~/composables/mode'
import { saveLastNetworkInstanceId, loadLastNetworkInstanceId } from '~/composables/config'
import ModeSwitcher from '~/components/ModeSwitcher.vue'
import { getEasytierVersion, getServiceStatus, resolveSharedConfigDir, retireConflictingServices, syncConfigsFromCore } from '~/composables/backend'

const { t, locale } = useI18n()
const confirm = useConfirm()
const aboutVisible = ref(false)
const modeDialogVisible = ref(false)
const modeDialogEpoch = ref(0)
const currentMode = ref<Mode>({ mode: 'normal' })
const editingMode = ref<Mode>({ mode: 'normal' })
const isModeSaving = ref(true)
const runtimeEpoch = ref(0)
let reconnectPromise: Promise<void> | undefined
let autoReconnectEnabled = true
let normalWebClientInitialized = false
const SERVICE_SCHEMA_VERSION = 2

const configServerDialogVisible = ref(false)
const configServerConnected = ref(false)

const showAutostartHint = ref(false)

async function openModeDialog() {
  editingMode.value = JSON.parse(JSON.stringify(loadMode()))
  modeDialogEpoch.value++
  showAutostartHint.value = false
  modeDialogVisible.value = true
}

async function openAutostartDialog() {
  editingMode.value = JSON.parse(JSON.stringify(loadMode()))
  editingMode.value.mode = 'service'
  modeDialogEpoch.value++
  showAutostartHint.value = true
  modeDialogVisible.value = true
}

async function onModeSave() {
  if (isModeSaving.value) {
    return false
  }
  isModeSaving.value = true
  autoReconnectEnabled = true
  if (reconnectPromise) await reconnectPromise
  const previousMode = JSON.parse(JSON.stringify(currentMode.value)) as Mode
  try {
    await initWithMode(JSON.parse(JSON.stringify(editingMode.value)) as Mode);
    modeDialogVisible.value = false
    return true
  }
  catch (e: any) {
    toast.add({ severity: 'error', summary: t('error'), detail: e, life: 10000 })
    console.error("Error switching mode", e, currentMode.value, editingMode.value)
    try {
      await initWithMode(previousMode);
    } catch (restoreError) {
      console.error('Failed to restore the previous mode', restoreError)
      clientRunning.value = false
    }
    return false
  }
  finally {
    isModeSaving.value = false
  }
}

async function onUninstallService() {
  confirm.require({
    message: t('mode.uninstall_service_confirm'),
    header: t('mode.uninstall_service'),
    icon: 'pi pi-exclamation-triangle',
    rejectProps: {
      label: t('web.common.cancel'),
      severity: 'secondary',
      outlined: true
    },
    acceptProps: {
      label: t('mode.uninstall_service'),
      severity: 'danger'
    },
    accept: async () => {
      if (isModeSaving.value) return
      isModeSaving.value = true
      autoReconnectEnabled = true
      if (reconnectPromise) await reconnectPromise
      try {
        const nextMode: Mode = currentMode.value.mode === 'normal'
          ? { ...currentMode.value }
          : {
              mode: 'normal',
              config_dir: currentMode.value.config_dir,
              config_server_url: currentMode.value.mode === 'service' ? currentMode.value.config_server_url : undefined,
            }
        await initWithMode(nextMode)
        toast.add({ severity: 'success', summary: t('web.common.success'), detail: t('mode.uninstall_service_success'), life: 3000 })
        modeDialogVisible.value = false
      } catch (e: any) {
        toast.add({ severity: 'error', summary: t('error'), detail: e, life: 10000 })
        console.error("Error uninstalling service", e)
      } finally {
        isModeSaving.value = false
      }
    },
  });
}

function stripModeMetadata(mode: Mode) {
  if (mode.mode !== 'service') {
    return mode
  }

  const serviceConfig = { ...mode }
  delete serviceConfig.installed_core_version
  delete serviceConfig.installed_service_schema
  return serviceConfig
}

function modeConfigChanged(next: Mode) {
  return JSON.stringify(stripModeMetadata(next)) !== JSON.stringify(stripModeMetadata(currentMode.value))
}

async function onStopService() {
  if (isModeSaving.value) return
  isModeSaving.value = true
  autoReconnectEnabled = false
  if (reconnectPromise) await reconnectPromise
  try {
    await waitForServiceStop()
    await invoke('stop_local_backend')
    normalWebClientInitialized = false
    clientRunning.value = false
    toast.add({ severity: 'success', summary: t('web.common.success'), detail: t('mode.stop_service_success'), life: 3000 })
    modeDialogVisible.value = false
  }
  catch (e: any) {
    autoReconnectEnabled = true
    toast.add({ severity: 'error', summary: t('error'), detail: e, life: 10000 })
    console.error("Error stopping service", e)
  }
  finally {
    isModeSaving.value = false
  }
}

const delay = (ms: number) => new Promise<void>(resolve => setTimeout(resolve, ms))

async function waitForServiceStop() {
  let stopRequested = false
  let stopError: unknown
  for (let i = 0; i < 300; i++) {
    const status = await getServiceStatus()
    if (status === 'Stopped' || status === 'NotInstalled') return
    if (!stopRequested) {
      try {
        await setServiceStatus(false)
        stopRequested = true
      } catch (error) {
        stopError = error
      }
    }
    await delay(100)
  }
  throw new Error(`Timed out waiting for the service to stop: ${String(stopError || '')}`)
}

async function prepareConfigDir(mode: Mode) {
  if (mode.mode !== 'remote' && type() !== 'android') {
    mode.config_dir = await resolveSharedConfigDir(mode.config_dir)
  }
}

function rpcUrl(mode: Mode): string | undefined {
  if (mode.mode === 'normal') return mode.rpc_portal
  if (mode.mode === 'remote') return mode.remote_rpc_address
  if (/^\d+$/.test(mode.rpc_portal.trim())) return `tcp://127.0.0.1:${mode.rpc_portal.trim()}`
  return (mode.rpc_portal.includes('://') ? mode.rpc_portal : `tcp://${mode.rpc_portal}`)
    .replace('0.0.0.0', '127.0.0.1').replace('[::]', '[::1]')
}

async function connectWithRetry(mode: Mode, attempts: number): Promise<boolean> {
  let lastError: unknown
  for (let attempt = 0; attempt < attempts; attempt++) {
    try {
      await connectRpcClient(mode)
      await remoteClient.value.list_network_instance_ids()
      if (mode.mode === 'service' && await getServiceStatus() !== 'Running') {
        throw new Error('The GUI service stopped before its RPC connection was ready')
      }
      return true
    } catch (error) {
      lastError = error
      if (attempt + 1 < attempts) await delay(1000)
    }
  }
  if (mode.mode === 'service') {
    if (await getServiceStatus() !== 'Running') {
      throw new Error(`The GUI service failed to start: ${String(lastError)}`)
    }
    console.warn('Service RPC is not ready yet; background reconnect will continue', lastError)
    return false
  }
  throw lastError
}

async function refreshFromCore() {
  if (type() === 'android') {
    await sendConfigs([], 'core-config-migrated:normal')
  } else {
    await syncConfigsFromCore().catch(error => console.warn('Failed to refresh GUI config cache', error))
  }
  try {
    const ids = await remoteClient.value.list_network_instance_ids()
    const available = [...(ids.running_inst_ids ?? []), ...(ids.disabled_inst_ids ?? [])]
      .map(Utils.UuidToStr)
    const preferred = instanceId.value || loadLastNetworkInstanceId()
    instanceId.value = preferred && available.includes(preferred) ? preferred : undefined
  } catch (error) {
    console.warn('Failed to refresh network selection', error)
    instanceId.value = undefined
  }
  runtimeEpoch.value++
}

async function initWithMode(mode: Mode) {
  await prepareConfigDir(mode)
  if (mode.mode === 'remote' && !mode.remote_rpc_address.trim()) {
    throw new Error(t('mode.remote_rpc_address_empty'))
  }
  if (mode.mode === 'service' && (!mode.config_dir || !mode.file_log_dir || !mode.file_log_level || !mode.rpc_portal)) {
    throw new Error(t('mode.service_config_empty'))
  }

  clientRunning.value = false
  if (mode.mode !== 'service' && type() !== 'android') {
    await waitForServiceStop()
    if (await getServiceStatus() !== 'NotInstalled') await initService(undefined)
    if (mode.mode === 'normal') {
      const retired = await retireConflictingServices(mode.config_dir!, mode.rpc_portal)
      if (retired.length) console.warn('Removed conflicting EasyTier services:', retired)
    }
  }
  if (mode.mode !== 'normal') {
    await invoke('stop_local_backend')
    normalWebClientInitialized = false
  }

  if (mode.mode === 'service') {
    if (type() !== 'android') {
      const retired = await retireConflictingServices(mode.config_dir, mode.rpc_portal)
      if (retired.length) console.warn('Removed conflicting EasyTier services:', retired)
    }
    let serviceStatus = await getServiceStatus()
    const coreVersion = await getEasytierVersion()
    const needsInstall = serviceStatus === 'NotInstalled'
      || currentMode.value.mode !== 'service'
      || modeConfigChanged(mode)
      || mode.installed_core_version !== coreVersion
      || mode.installed_service_schema !== SERVICE_SCHEMA_VERSION
    if (needsInstall) {
      if (serviceStatus === 'Running') await waitForServiceStop()
      await initService({
        config_dir: mode.config_dir,
        file_log_dir: mode.file_log_dir,
        file_log_level: mode.file_log_level,
        rpc_portal: mode.rpc_portal,
        config_server: mode.config_server_url || undefined,
      })
      mode.installed_core_version = coreVersion
      mode.installed_service_schema = SERVICE_SCHEMA_VERSION
      serviceStatus = await getServiceStatus()
    }
    if (serviceStatus === 'Stopped') await setServiceStatus(true, mode.rpc_portal)
  }

  if (mode.mode === 'normal') normalWebClientInitialized = false
  const connected = await connectWithRetry(mode, mode.mode === 'service' ? 5 : 3)
  if (connected) {
    await refreshFromCore()
    if (mode.mode === 'normal') {
      await initWebClient(mode.config_server_url || undefined)
      normalWebClientInitialized = true
    }
  }
  currentMode.value = mode
  saveMode(mode)
  clientRunning.value = connected && await isClientRunning()
}

onMounted(async () => {
  const cleanupFns: Array<() => void> = []

  if (type() === 'android') {
    try {
      await initMobileVpnService()
    } catch (e: any) {
      console.error("easytier init vpn service failed", e)
    }
  }

  cleanupFns.push(await listenGlobalEvents())
  currentMode.value = loadMode()
  isModeSaving.value = true
  try {
    await initWithMode(currentMode.value)
  } catch (error) {
    clientRunning.value = false
    console.error('Failed to initialize saved mode', error)
    toast.add({ severity: 'error', summary: t('error'), detail: String(error), life: 10000 })
  } finally {
    isModeSaving.value = false
  }

  if (type() === 'android') {
    setMobileVpnTileActionHandler(handleMobileVpnTileAction)
    cleanupFns.push(() => setMobileVpnTileActionHandler())
    try {
      await consumePendingMobileVpnTileAction()
      await syncMobileVpnService()
    } catch (e: any) {
      console.error("easytier sync vpn service failed", e)
    }
  }

  onUnmounted(() => {
    cleanupFns.forEach(unlisten => unlisten())
  })
});

useTray(true)
let toast = useToast();

const remoteClient = computed(() => new GUIRemoteClient());
const instanceId = ref<string | undefined>(undefined);
const clientRunning = ref(false);

async function handleMobileVpnTileAction(action: 'start' | 'stop') {
  try {
    const result = await executeVpnTileAction(action, remoteClient.value, {
      lastInstanceId: loadLastNetworkInstanceId(),
      syncVpnService: syncMobileVpnService,
    })

    if (!result.instanceId) {
      toast.add({
        severity: 'warn',
        summary: t('vpn_tile_no_network'),
        detail: t('vpn_tile_no_network_description'),
        life: 5000,
      })
      return
    }

    instanceId.value = result.instanceId
    saveLastNetworkInstanceId(result.instanceId)
    toast.add({
      severity: action === 'start' ? 'success' : 'secondary',
      summary: t(action === 'start' ? 'vpn_tile_started' : 'vpn_tile_stopped'),
      life: 3000,
    })
  }
  catch (error) {
    console.error('VPN tile action failed', action, error)
    toast.add({
      severity: 'error',
      summary: t('error'),
      detail: t('vpn_tile_action_failed', { error: String(error) }),
      life: 8000,
    })
  }
}

watch(instanceId, (newVal) => {
  if (newVal) {
    saveLastNetworkInstanceId(newVal);
  }
});

async function reconnectRpc() {
  if (isModeSaving.value || !autoReconnectEnabled || reconnectPromise) return
  reconnectPromise = (async () => {
    try {
      await connectRpcClient(currentMode.value)
      await remoteClient.value.list_network_instance_ids()
      if (currentMode.value.mode === 'service' && await getServiceStatus() !== 'Running') {
        throw new Error('The GUI service stopped during RPC reconnect')
      }
      await refreshFromCore()
      if (currentMode.value.mode === 'normal' && !normalWebClientInitialized) {
        await initWebClient(currentMode.value.config_server_url || undefined)
        normalWebClientInitialized = true
      }
      clientRunning.value = await isClientRunning()
    } catch (error) {
      clientRunning.value = false
      console.debug('RPC reconnect will be retried', error)
    }
  })().finally(() => { reconnectPromise = undefined })
  await reconnectPromise
}

onMounted(() => {
  const timer = setInterval(async () => {
    if (isModeSaving.value || reconnectPromise || !autoReconnectEnabled) return
    const running = await isClientRunning().catch(() => false)
    if (running && clientRunning.value) return
    clientRunning.value = false
    await reconnectRpc()
  }, 1500)

  onUnmounted(() => {
    clearInterval(timer)
  })
})
async function reconnectClient() {
  editingMode.value = JSON.parse(JSON.stringify(loadMode()));
  await onModeSave()
}

onMounted(async () => {
  window.setTimeout(async () => {
    await setTrayMenu([
      await MenuItemShow(t('tray.show')),
      await MenuItemExit(t('tray.exit')),
    ])
  }, 1000)
})

let current_log_level = 'off'

const log_menu = ref()
// 从后端获取正确的日志路径
async function getLogDirPath(): Promise<string> {
  return await invoke<string>('get_log_dir_path')
}

const log_menu_items_popup: Ref<MenuItem[]> = ref([
  ...['off', 'warn', 'info', 'debug', 'trace'].map(level => ({
    label: () => t(`logging_level_${level}`) + (current_log_level === level ? ' ✓' : ''),
    command: async () => {
      current_log_level = level
      await setLoggingLevel(level)
    },
  })),
  {
    separator: true,
  },
  {
    label: () => t('logging_open_dir'),
    icon: 'pi pi-folder-open',
    command: async () => {
      // console.log('open log dir', await getLogDirPath())
      await open(await getLogDirPath())
    },
    visible: () => type() !== 'android',
  },
  {
    label: () => t('logging_copy_dir'),
    icon: 'pi pi-tablet',
    command: async () => {
      await writeText(await getLogDirPath())
    },
  },
])

function toggle_log_menu(event: any) {
  log_menu.value.toggle(event)
}

function getLabel(item: MenuItem) {
  return typeof item.label === 'function' ? item.label() : item.label
}

const setting_menu_items: Ref<MenuItem[]> = ref([
  {
    label: () => t('exchange_language'),
    icon: 'pi pi-language',
    command: async () => {
      await I18nUtils.loadLanguageAsync((locale.value === 'en' ? 'cn' : 'en'))
      await setTrayMenu([
        await MenuItemShow(t('tray.show')),
        await MenuItemExit(t('tray.exit')),
      ])
    },
  },
  {
    label: () => `${t('mode.switch_mode')}: ${t('mode.' + currentMode.value.mode)}`,
    icon: 'pi pi-sync',
    command: openModeDialog,
    visible: () => type() !== 'android',
  },
  {
    label: () => t('mode.autostart'),
    icon: 'pi pi-clock',
    command: openAutostartDialog,
    visible: () => type() !== 'android',
  },
  {
    label: () => `${t('config-server.title')}${t('config-server.' + configServerConnectionStatus.value)}`,
    icon: 'pi pi-globe',
    command: openConfigServerDialog,
    visible: () => ["normal", "service"].includes(currentMode.value.mode),
  },
  {
    key: 'logging_menu',
    label: () => t('logging'),
    icon: 'pi pi-file',
    items: [], // Keep this to show it's a parent menu
  },
  {
    label: () => t('about.title'),
    icon: 'pi pi-at',
    command: async () => {
      aboutVisible.value = true
    },
  },
  {
    label: () => t('exit'),
    icon: 'pi pi-power-off',
    command: async () => {
      await exit(1)
    },
  },
])

async function connectRpcClient(mode: Mode) {
  await initRpcConnection(mode.mode === 'normal', rpcUrl(mode), mode.mode === 'normal' ? mode.config_dir : undefined)
  console.log('easytier rpc connection established, mode:', mode.mode)
}

async function openConfigServerDialog() {
  editingMode.value = JSON.parse(JSON.stringify(loadMode()))
  configServerDialogVisible.value = true
}
async function onConfigServerSave() {
  if (JSON.stringify(currentMode.value) === JSON.stringify(editingMode.value)) {
    configServerDialogVisible.value = false
    return;
  }
  if (editingMode.value.mode === 'service') {
    const confirmed = await new Promise<boolean>((resolve) => {
      confirm.require({
        message: t('config-server.update_service_confirm'),
        icon: 'pi pi-exclamation-triangle',
        rejectProps: {
          label: t('web.common.cancel'),
          severity: 'secondary',
          outlined: true
        },
        acceptProps: {
          label: t('web.common.confirm'),
        },
        accept: async () => {
          resolve(true)
        },
        reject: () => {
          resolve(false)
        }
      });
    })
    if (!confirmed) return
  }
  console.log("Saving config server url", (editingMode.value as WebClientConfig).config_server_url)
  if (await onModeSave()) configServerDialogVisible.value = false
}
onMounted(() => {
  const timer = setInterval(async () => {
    if (currentMode.value.mode !== 'normal') return;
    if (!currentMode.value.config_server_url) return;
    configServerConnected.value = await isWebClientConnected();
  }, 1000)

  onUnmounted(() => {
    clearInterval(timer)
  })
})
const configServerConnectionStatus = computed(() => {
  if (currentMode.value.mode !== 'normal') {
    return 'unknown'
  }
  if (!currentMode.value.config_server_url) {
    return 'disconnected'
  }
  return configServerConnected.value ? 'connected' : 'connecting'
})

</script>

<template>
  <div id="root" class="flex flex-col">
    <Dialog v-model:visible="aboutVisible" modal :header="t('about.title')" :style="{ width: '70%' }">
      <About />
    </Dialog>
    <Dialog v-model:visible="modeDialogVisible" modal :header="t('mode.switch_mode')" :style="{ width: '50vw' }">
      <Message v-if="showAutostartHint" severity="info" :closable="false" class="mb-4">
        {{ t('mode.autostart_hint') }}
      </Message>
      <ModeSwitcher :key="modeDialogEpoch" v-model="editingMode" @uninstall-service="onUninstallService" @stop-service="onStopService" />
      <template #footer>
        <Button :label="t('web.common.cancel')" icon="pi pi-times" @click="modeDialogVisible = false" text />
        <Button :label="t('web.common.save')" icon="pi pi-save" @click="onModeSave" autofocus :loading="isModeSaving" />
      </template>
    </Dialog>

    <Dialog v-model:visible="configServerDialogVisible" modal :header="t('config-server.title')"
      :style="{ width: '50vw' }">
      <div class="flex flex-col gap-3">
        <label for="config-server-address">{{ t('config-server.address') }}</label>
        <Textarea id="config-server-address" v-model="(editingMode as WebClientConfig).config_server_url"
          :placeholder="t('config-server.address_placeholder')" rows="3" auto-resize />
        <small class="p-text-secondary whitespace-pre-wrap">{{ t('config-server.description') }}</small>
      </div>
      <template #footer>
        <Button :label="t('web.common.cancel')" icon="pi pi-times" @click="configServerDialogVisible = false" text />
        <Button :label="t('web.common.save')" icon="pi pi-save" @click="onConfigServerSave" autofocus
          :loading="isModeSaving" />
      </template>
    </Dialog>

    <Menu ref="log_menu" :model="log_menu_items_popup" :popup="true" />

    <RemoteManagement v-if="clientRunning" :key="runtimeEpoch" class="flex-1 overflow-y-auto" :api="remoteClient"
      :pause-auto-refresh="isModeSaving" v-model:instance-id="instanceId" />
    <div v-else class="empty-state flex-1 flex flex-col items-center py-12">
      <i class="pi pi-server text-5xl text-secondary mb-4 opacity-50"></i>
      <div class="text-xl text-center font-medium mb-3">{{ t('client.not_running') }}
      </div>
      <Button @click="reconnectClient" :loading="isModeSaving" :label="t('client.retry')" icon="pi pi-replay"
        iconPos="left" />
    </div>

    <Menubar :model="setting_menu_items" breakpoint="795px">
      <template #item="{ item, props }">
        <a v-if="item.key === 'logging_menu'" v-bind="props.action" @click="toggle_log_menu">
          <span :class="item.icon" />
          <span class="p-menubar-item-label">{{ getLabel(item) }}</span>
          <span class="pi pi-angle-down p-menubar-item-icon text-[9px]"></span>
        </a>
        <a v-else v-bind="props.action">
          <span :class="item.icon" />
          <span class="p-menubar-item-label">{{ getLabel(item) }}</span>
        </a>
      </template>
    </Menubar>
  </div>
</template>

<style scoped lang="postcss">
#root {
  height: 100vh;
  width: 100vw;
}

.p-dropdown :deep(.p-dropdown-panel .p-dropdown-items .p-dropdown-item) {
  padding: 0 0.5rem;
}
</style>

<style>
body {
  height: 100vh;
  width: 100vw;
  padding: 0;
  margin: 0;
  overflow: hidden;
}

.p-menubar .p-menuitem {
  margin: 0;
}

.p-select-overlay {
  max-width: calc(100% - 2rem);
}

/*

.p-tabview-panel {
  height: 100%;
} */
</style>
