<script setup lang="ts">
import { applyThemeMode, storedThemeMode, type ThemeMode } from '../modules/theme';

const themeMode = ref<ThemeMode>(storedThemeMode());
const themeCycle = [
    { mode: 'system' as ThemeMode, icon: 'pi pi-desktop' },
    { mode: 'light' as ThemeMode, icon: 'pi pi-sun' },
    { mode: 'dark' as ThemeMode, icon: 'pi pi-moon' },
];
const themeIcon = computed(() => themeCycle.find(entry => entry.mode === themeMode.value)?.icon ?? 'pi pi-desktop');
const cycleThemeMode = () => {
    const index = themeCycle.findIndex(entry => entry.mode === themeMode.value);
    const next = themeCycle[(index + 1) % themeCycle.length].mode;
    themeMode.value = next;
    applyThemeMode(next);
};

import { I18nUtils } from 'easytier-frontend-lib'
import { computed, onMounted, onUnmounted, ref, watch } from 'vue';
import { Button, Dialog, Drawer, OverlayBadge, Popover, TieredMenu, useToast } from 'primevue';
import { useRoute, useRouter } from 'vue-router';
import { useDialog } from 'primevue/usedialog';
import ChangePassword from './ChangePassword.vue';
import DevicePopup from './DevicePopup.vue';
import ConsoleNav from './ConsoleNav.vue';
import Icon from '../assets/easytier.png'
import { useI18n } from 'vue-i18n'
import ApiClient, { type BlockedDevice } from '../modules/api';
import { createNetworkDiscovery, provideNetworkDiscovery } from '../modules/networkDiscovery';
import { NETWORK_SECTIONS } from '../modules/meshNetworkContext';
import { getApiBase } from '../modules/api-host';

const { t } = useI18n()
const router = useRouter();
const toast = useToast();
const route = useRoute();
const api = new ApiClient(getApiBase(), () => router.push({ name: 'login' }));
const dialog = useDialog();
const userMenu = ref();
const showCredits = ref(false);
const userMenuItems = computed(() => [
    {
        label: t('web.main.about'),
        icon: 'pi pi-info-circle',
        command: () => { showCredits.value = true; },
    },
    {
        label: t('web.main.change_password'),
        icon: 'pi pi-key',
        command: () => dialog.open(ChangePassword, {
            props: { modal: true, header: t('web.main.change_password') },
            data: { api },
        }),
    },
    {
        label: t('web.main.logout'),
        icon: 'pi pi-sign-out',
        command: async () => {
            try {
                await api.logout();
            } catch (e) {
                console.error('logout failed', e);
            }
            router.push({ name: 'login' });
        },
    },
]);

// Networks are first-class navigation targets: each network gets a direct
// entry so managing one is a single click from anywhere. They are recognized
// from the devices' own configurations; every page shares this one scan.
const discovery = createNetworkDiscovery(api, () => centralEnabled.value);
provideNetworkDiscovery(discovery);
const networks = computed(() => discovery.networks.value ?? []);
// Blocked devices that still try to come online raise the bell badge.
const blockedDevices = ref<BlockedDevice[]>([]);
const loadBlockedDevices = async () => {
    if (!centralEnabled.value) return;
    try {
        blockedDevices.value = (await api.list_blocked_devices()) ?? [];
    } catch (e) {
        // Notifications are best-effort.
    }
};
let networkTimer: number | undefined;
const loadNavigation = async () => {
    if (!enrollInfo.value) {
        try {
            enrollInfo.value = await api.get_console_info();
        } catch (error) {
            console.error('Failed to load console mode', error);
            return;
        }
    }
    await loadBlockedDevices();
};
onMounted(() => {
    loadNavigation();
    networkTimer = window.setInterval(loadNavigation, 10_000);
    discovery.start();
});
onUnmounted(() => {
    window.clearInterval(networkTimer);
    discovery.stop();
});

// Device enrollment info: the config-server command devices run to join
// this console. Also identifies whether central management is available.
const enrollBell = ref();
const enrollInfo = ref<import('../modules/api').ConsoleInfo | null>(null);
const enrollLoading = ref(false);
const centralEnabled = computed(() => enrollInfo.value?.webhook_auth === false);
// Central networks join the recognized ones as soon as the mode is known.
watch(centralEnabled, enabled => { if (enabled) void discovery.refresh(); });

const enrollCommand = computed(() => {
    if (!enrollInfo.value) return '';
    const host = new URL(getApiBase(), window.location.href).hostname;
    const { config_server_protocol: proto, config_server_port: port, username } = enrollInfo.value;
    return `easytier-core --config-server ${proto}://${host}:${port}/${username}`;
});

const toggleEnrollBell = async (event: Event) => {
    enrollBell.value.toggle(event);
    if (!enrollInfo.value && !enrollLoading.value) {
        enrollLoading.value = true;
        try {
            enrollInfo.value = await api.get_console_info();
        } catch (e) {
            console.error(e);
        } finally {
            enrollLoading.value = false;
        }
    }
};

const copyEnrollCommand = async () => {
    try {
        await navigator.clipboard.writeText(enrollCommand.value);
        toast.add({ severity: 'success', summary: t('web.common.confirm'), life: 1500 });
    } catch (e) {
        console.error(e);
    }
};

const blockedAttemptCount = computed(() =>
    blockedDevices.value.filter(device => device.attempt_count > 0).length);
const blockBell = ref();
const toggleBlockBell = (event: Event) => blockBell.value.toggle(event);
const unblockDevice = async (device: BlockedDevice) => {
    try {
        await api.unblock_device(device.id);
        await loadBlockedDevices();
    } catch (e: any) {
        toast.add({ severity: 'error', summary: t('web.console.blocked_devices'), detail: e?.response?.data?.message ?? String(e), life: 5000 });
    }
};

// The trail names where a page sits: its area, then the network and its page.
const breadcrumb = computed<string[]>(() => {
    if (route.params.networkName) {
        const name = String(route.params.networkName);
        const section = NETWORK_SECTIONS.find(entry => entry.name === route.name);
        return [
            t('web.console.nav_networks'),
            networks.value.find(item => item.name === name)?.label ?? name,
            ...(section ? [t(section.label)] : []),
        ];
    }
    if (route.name === 'networkList') return [t('web.console.nav_networks'), t('web.console.all_networks')];
    return [t('web.console.workspace'), route.name === 'deviceList' ? t('web.main.device_list') : t('web.main.dashboard')];
});
const forceShowSideBar = ref(false);
const mainContent = ref<HTMLElement>();

// 切换页面后关闭移动端侧边栏。
watch(() => route.fullPath, () => { forceShowSideBar.value = false; });
</script>

<template>
    <div class="web-console">
        <a class="skip-link" href="#main-content" @click.prevent="mainContent?.focus()">{{ t('web.console.skip_content') }}</a>
        <aside class="console-sidebar">
            <RouterLink :to="{ name: 'dashboard' }" class="console-brand">
                <img :src="Icon" alt="" /><span class="console-brand-name">EasyTier<span class="brand-caption">{{ t('web.console.console') }}</span></span>
            </RouterLink>
            <div class="console-sidebar-scroll">
                <ConsoleNav :networks="networks" :central-enabled="centralEnabled" />
            </div>
            <a href="https://github.com/225284228a-droid/EasyTier-Custom" target="_blank" rel="noopener noreferrer" class="console-docs">
                <i class="pi pi-book" aria-hidden="true"></i>{{ t('web.console.documentation') }}<i class="pi pi-arrow-up-right" aria-hidden="true"></i>
            </a>
        </aside>
        <Drawer v-model:visible="forceShowSideBar" class="console-mobile-nav">
            <template #header>
                <span class="console-brand console-brand-drawer"><img :src="Icon" alt="" /><span class="console-brand-name">EasyTier<span class="brand-caption">{{ t('web.console.console') }}</span></span></span>
            </template>
            <ConsoleNav :networks="networks" :central-enabled="centralEnabled" @navigate="forceShowSideBar = false" />
        </Drawer>
        <div class="console-workspace">
            <header class="console-topbar">
                <div class="flex items-center gap-3 min-w-0">
                    <Button icon="pi pi-bars" text severity="secondary" class="mobile-nav-toggle"
                        :aria-label="t('web.console.navigation')" @click="forceShowSideBar = true" />
                    <nav class="console-breadcrumb" :aria-label="t('web.console.breadcrumb')">
                        <template v-for="(crumb, index) in breadcrumb" :key="index">
                            <i v-if="index" class="pi pi-angle-right" aria-hidden="true"></i>
                            <span :class="{ 'is-current': index === breadcrumb.length - 1 }"
                                :aria-current="index === breadcrumb.length - 1 ? 'page' : undefined">{{ crumb }}</span>
                        </template>
                    </nav>
                </div>
                <div class="flex items-center gap-2">
                    <Button icon="pi pi-objects-column" :label="t('web.console.enroll_title')"
                        severity="secondary" outlined size="small" class="enroll-button"
                        :aria-label="t('web.console.enroll_title')"
                        @click="toggleEnrollBell" />
                    <Popover ref="enrollBell" appendTo="body" style="min-width: 34rem">
                        <div class="flex flex-col gap-2">
                            <span class="font-semibold">{{ t('web.console.enroll_title') }}</span>
                            <p class="text-sm muted m-0">{{ t('web.console.enroll_hint') }}</p>
                            <div v-if="enrollLoading" class="text-sm muted">{{ t('web.common.loading') }}</div>
                            <template v-else-if="enrollInfo">
                                <div class="flex items-center gap-2">
                                    <code class="mono-value flex-1 p-2 rounded"
                                        style="background: var(--console-ground); word-break: break-all">{{ enrollCommand }}</code>
                                    <Button icon="pi pi-copy" severity="secondary" outlined size="small"
                                        :aria-label="t('web.console.enroll_copy')" @click="copyEnrollCommand" />
                                </div>
                                <p class="text-xs muted m-0">
                                    {{ enrollInfo.webhook_auth ? t('web.console.enroll_webhook_note') : t('web.console.enroll_token_note') }}
                                </p>
                            </template>
                        </div>
                    </Popover>
                    <OverlayBadge v-if="centralEnabled" :value="blockedAttemptCount > 0 ? String(blockedAttemptCount) : undefined">
                        <Button icon="pi pi-bell" text severity="secondary" :aria-label="t('web.console.blocked_devices')" @click="toggleBlockBell" />
                    </OverlayBadge>
                    <Popover ref="blockBell" appendTo="body" style="min-width: 22rem">
                        <div class="flex items-center justify-between mb-2">
                            <span class="font-semibold">{{ t('web.console.blocked_devices') }}</span>
                        </div>
                        <div v-if="blockedDevices.length === 0" class="text-sm muted p-2">{{ t('web.console.blocked_empty') }}</div>
                        <div v-else class="flex flex-col gap-2">
                            <div v-for="device in blockedDevices" :key="device.id" class="flex items-center justify-between gap-3 border-b border-surface last:border-b-0 pb-2 last:pb-0">
                                <div class="min-w-0">
                                    <div class="font-medium truncate">{{ device.alias || device.hostname || device.id }}</div>
                                    <div class="text-xs muted">
                                        <template v-if="device.attempt_count > 0">
                                            {{ t('web.console.blocked_attempted', { time: (device.last_attempt_time || '').replace('T', ' ').slice(0, 19), count: device.attempt_count }) }}
                                        </template>
                                        <template v-else>{{ t('web.console.blocked_no_attempt') }}</template>
                                    </div>
                                </div>
                                <Button :label="t('web.console.unblock')" size="small" severity="secondary" outlined @click="unblockDevice(device)" />
                            </div>
                        </div>
                    </Popover>
                    <Button :icon="themeIcon" text severity="secondary"
                        :aria-label="t('web.console.theme_mode')" v-tooltip.bottom="t('web.console.theme_mode')"
                        @click="cycleThemeMode" />
                    <Button icon="pi pi-language" text severity="secondary" :aria-label="t('web.console.language')" @click="I18nUtils.toggleLanguage" />
                    <span class="topbar-divider"></span>
                    <Button type="button" @click="userMenu.toggle($event)" aria-haspopup="true" aria-controls="user-menu"
                        :aria-label="t('web.console.account')" icon="pi pi-user" text severity="secondary" />
                    <TieredMenu ref="userMenu" id="user-menu" :model="userMenuItems" popup />
                </div>
            </header>
            <main ref="mainContent" id="main-content" class="console-content" tabindex="-1">
                <RouterView v-slot="{ Component }">
                    <!-- The device popup lives in the query, so opening and closing it
                         keeps the page mounted where it was. -->
                    <component :is="Component" :key="`${api.persistenceScope}:${route.params.networkName ?? ''}`" :api="api" :central-enabled="centralEnabled" />
                </RouterView>
            </main>
            <DevicePopup :api="api" :central-enabled="centralEnabled" />
        </div>
    </div>
    <Dialog v-model:visible="showCredits" :header="t('web.main.about')" modal
        :style="{ width: '28rem', maxWidth: 'calc(100vw - 32px)' }">
        <div class="geoip-attribution">
            IP geolocation by <a href="https://db-ip.com/" target="_blank" rel="noopener noreferrer">DB-IP</a>
            (<a href="https://creativecommons.org/licenses/by/4.0/" target="_blank"
                rel="noopener noreferrer">CC BY 4.0</a>)
        </div>
        <div class="geoip-attribution">
            <a href="https://www.geojs.io/" target="_blank" rel="noopener noreferrer">GeoJS</a>,
            <a href="https://www.maxmind.com/" target="_blank" rel="noopener noreferrer">MaxMind GeoLite</a>,
            <a href="https://ipwhois.io/" target="_blank" rel="noopener noreferrer">IPWho.is</a>
        </div>
    </Dialog>
</template>

<style scoped>
.sidebar-button {
    text-align: left;
    justify-content: left;
}
.geoip-attribution {
    margin-bottom: 12px;
    color: var(--p-text-muted-color);
    font-size: 11px;
}
.geoip-attribution a {
    text-decoration: underline;
}
</style>
