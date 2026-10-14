import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import PrimeVue from 'primevue/config'
import { afterEach, describe, expect, it, vi } from 'vitest'
import Status from '../src/components/Status.vue'
import type { NetworkInstance } from '../src/types/network'

vi.mock('vue-i18n', () => ({ useI18n: () => ({ t: (key: string) => key }) }))

const wrappers: VueWrapper[] = []

afterEach(() => {
  wrappers.splice(0).forEach(wrapper => wrapper.unmount())
})

async function render(localStun: unknown, peerStun: unknown, lossRate?: number, vpnStatus?: unknown) {
  const instance = {
    instance_id: '12345678-9abc-def0-fedc-ba9876543210',
    running: true,
    detail: {
      my_node_info: {
        hostname: 'local-node',
        version: 'test',
        peer_id: 1,
        stun_info: localStun,
      },
      peer_route_pairs: [{
        route: {
          hostname: 'remote-node',
          version: 'test',
          cost: 1,
          stun_info: peerStun,
        },
        peer: {
          conns: [{
            conn_id: 'connection',
            ...(lossRate === undefined ? {} : { loss_rate: lossRate }),
          }],
        },
      }],
    },
  } as unknown as NetworkInstance
  const wrapper = mount(Status, {
    props: {
      curNetworkInst: instance,
      api: {
        get_network_config: vi.fn(async () => ({})),
        get_vpn_status: vi.fn(async () => vpnStatus),
      } as any,
    },
    global: {
      plugins: [PrimeVue],
      directives: { tooltip: () => {} },
      stubs: { HumanEvent: true, NetworkChart: true, VpnPortalDialog: true },
    },
  })
  wrappers.push(wrapper)
  await flushPromises()
  return wrapper
}

describe('status protobuf JSON rendering', () => {
  it('renders enum names and omitted zero loss in the real peer table', async () => {
    const wrapper = await render({ udp_nat_type: 'FullCone' }, { udp_nat_type: 'PortRestricted' })
    const rows = wrapper.findAll('tbody tr')
    expect(rows[0].text()).toContain('Full Cone')
    expect(rows[0].text()).not.toContain('0%')
    expect(rows[1].text()).toContain('Port Restricted')
    expect(rows[1].text()).toContain('0%')
    await wrapper.findAll('button').find(button => button.text() === 'show_node_details')!.trigger('click')
    expect(wrapper.text()).toContain('UDP NAT Type: Full Cone')
    expect(wrapper.text()).not.toContain('UDP NAT Type: undefined')
  })

  it('renders omitted enum defaults as Unknown but leaves missing STUN data blank', async () => {
    const wrapper = await render({}, undefined, 0.25)
    const rows = wrapper.findAll('tbody tr')
    expect(rows[0].text()).toContain('Unknown')
    expect(rows[1].text()).not.toContain('Unknown')
    expect(rows[1].text()).toContain('25%')
    expect(wrapper.text()).toContain('UDP NAT Type: Unknown')
  })

  it('preserves numeric NAT values from older backends', async () => {
    const wrapper = await render({ udp_nat_type: 1 }, { udp_nat_type: 6 }, 0.5)
    const rows = wrapper.findAll('tbody tr')
    expect(rows[0].text()).toContain('Open Internet')
    expect(rows[1].text()).toContain('Symmetric')
    expect(rows[1].text()).toContain('50%')
  })

  it('shows each subnet, single exit and availability', async () => {
    const wrapper = await render(undefined, undefined, undefined, {
      enabled: true, exit_enabled: false,
      rules: [
        { cidr: '203.0.113.0/24', exit_addr: '10.9.0.2', state: 'down' },
        { cidr: '198.51.100.0/24', exit_addr: '10.9.0.3', state: 'up' },
      ],
      routes_active: true, routes_installed: 2,
      capture_routes: [], policy_generation: 1n, ipv6_addr: '',
    })
    const text = wrapper.text()
    for (const value of ['203.0.113.0/24', '198.51.100.0/24', '10.9.0.2', '10.9.0.3', 'vpn_exit_state_down', 'vpn_exit_state_up']) {
      expect(text).toContain(value)
    }
    for (const value of ['vpn_rule_effective', 'vpn_ignore_private', 'vpn_rule_packets', 'vpn_rule_priority']) {
      expect(text).not.toContain(value)
    }
  })

  it('shows a nonempty VPN activation error even when no rules are active', async () => {
    const error = 'TUN route installation failed'
    const wrapper = await render(undefined, undefined, undefined, {
      enabled: false,
      exit_enabled: false,
      rules: [],
      routes_active: false,
      routes_installed: 0,
      capture_routes: [],
      policy_generation: 0n,
      ipv6_addr: '',
      activation_error: error,
    })

    const alert = wrapper.find('[role="alert"]')
    expect(alert.exists()).toBe(true)
    expect(alert.text()).toBe(error)
  })
})
