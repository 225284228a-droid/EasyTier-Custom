import { createApp, defineComponent, h, reactive } from 'vue'
import PrimeVue from 'primevue/config'
import Aura from '@primeuix/themes/aura'
import { vTooltip } from 'floating-vue'
import Config from '../../src/components/Config.vue'
import { DEFAULT_NETWORK_CONFIG, type NetworkConfig } from '../../src/types/network'
import I18nUtils from '../../src/modules/i18n'
import '../../src/style.css'

await I18nUtils.loadLanguageAsync('cn')

const BrowserHarness = defineComponent({
  setup() {
    const network = reactive(DEFAULT_NETWORK_CONFIG())
    network.network_name = 'vpn-responsive-test'
    network.network_secret = 'browser-test-secret'
    network.vpn = {
      exit_enabled: false,
      rules: [],
    }

    return () => h('main', { class: 'vpn-test-shell min-w-0 w-full' }, [
      h(Config, {
        curNetwork: network,
        hostname: 'browser-vpn-test',
        'onUpdate:curNetwork': (value: NetworkConfig) => Object.assign(network, value),
      }),
    ])
  },
})

const app = createApp(BrowserHarness)
app.use(I18nUtils.i18n)
app.use(PrimeVue, {
  ripple: false,
  theme: {
    preset: Aura,
    options: {
      prefix: 'p',
      darkModeSelector: '.app-dark',
      cssLayer: {
        name: 'primevue',
        order: 'tailwind-base, primevue, tailwind-utilities',
      },
    },
  },
})
app.directive('tooltip', vTooltip)
app.mount('#app')
