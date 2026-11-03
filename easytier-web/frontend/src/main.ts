import { createApp } from 'vue'
import 'easytier-frontend-lib/style.css'
import './style.css'
import './console.css'
import App from './App.vue'
import EasytierFrontendLib from 'easytier-frontend-lib'
import PrimeVue from 'primevue/config'
import ConsoleTheme from './theme';
import { initThemeMode } from './modules/theme';
import ConfirmationService from 'primevue/confirmationservice';
import { I18nUtils } from 'easytier-frontend-lib'

import { createRouter, createWebHashHistory } from 'vue-router'
import MainPage from './components/MainPage.vue'
import Login from './components/Login.vue'
import DeviceList from './components/DeviceList.vue'
import Dashboard from './components/Dashboard.vue'
import NetworkList from './components/NetworkList.vue'
import NetworkShell from './components/network/NetworkShell.vue'
import NetworkOverview from './components/network/NetworkOverview.vue'
import NetworkMembers from './components/network/NetworkMembers.vue'
import NetworkSettings from './components/network/NetworkSettings.vue'
import NetworkAdvanced from './components/network/NetworkAdvanced.vue'
import NetworkAcl from './components/network/NetworkAcl.vue'
import NetworkPreset from './components/network/NetworkPreset.vue'
import NetworkTemporary from './components/network/NetworkTemporary.vue'
import DialogService from 'primevue/dialogservice';
import ToastService from 'primevue/toastservice';

const routes = [
    {
        path: '/auth', children: [
            {
                name: 'login',
                path: '',
                component: Login,
                alias: 'login',
                props: { isRegistering: false }
            },
            {
                name: 'register',
                path: 'register',
                component: Login,
                props: { isRegistering: true }
            }
        ]
    },
    {
        path: '/h', component: MainPage, children: [
            {
                path: '',
                alias: 'dashboard',
                name: 'dashboard',
                component: Dashboard,
            },
            {
                path: 'deviceList',
                name: 'deviceList',
                component: DeviceList,
            },
            {
                // Device management opens over the page it was opened from.
                path: 'deviceList/device/:deviceId/:instanceId?',
                name: 'deviceManagement',
                redirect: (to: any) => ({
                    name: 'deviceList',
                    query: { device: to.params.deviceId, instance: to.params.instanceId || undefined },
                }),
            },
            {
                path: 'networks',
                name: 'networkList',
                component: NetworkList,
            },
            {
                path: 'networks/view/:networkName',
                component: NetworkShell,
                children: [
                    { path: '', name: 'meshNetwork', component: NetworkOverview },
                    { path: 'members', name: 'meshMembers', component: NetworkMembers },
                    { path: 'settings', name: 'meshSettings', component: NetworkSettings },
                    { path: 'advanced', name: 'meshAdvanced', component: NetworkAdvanced },
                    { path: 'acl', name: 'meshAcl', component: NetworkAcl },
                    { path: 'preset', name: 'meshPreset', component: NetworkPreset },
                    { path: 'temporary', name: 'meshTemporary', component: NetworkTemporary },
                ],
            },
            {
                // A network has one page; central records no longer have their own.
                path: 'networks/:networkId',
                redirect: { name: 'networkList' },
            },
        ]
    },
    {
        path: '/:pathMatch(.*)*', name: 'notFound', redirect: { name: 'dashboard' }
    }
]

const router = createRouter({
    history: createWebHashHistory(),
    routes,
})

const app = createApp(App)

// Use i18n
app.use(I18nUtils.i18n)
// Apply the Web-specific PrimeVue theme first; register the shared
// component library without its built-in theme so our dark mode selector
// is the only one that takes effect.
app.use(PrimeVue,
    {
        theme: {
            preset: ConsoleTheme,
            options: {
                prefix: 'p',
                darkModeSelector: '.app-dark',
                cssLayer: {
                    name: 'primevue',
                    order: 'tailwind-base, primevue, tailwind-utilities'
                }
            }
        }
    }
).use(ToastService as any).use(DialogService as any).use(router).use(ConfirmationService as any)

app.use(EasytierFrontendLib, { skipPrimeVue: true })

initThemeMode()
app.mount('#app')
