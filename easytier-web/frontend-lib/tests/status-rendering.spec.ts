import { flushPromises, mount, type VueWrapper } from '@vue/test-utils'
import PrimeVue from 'primevue/config'
import { afterEach, describe, expect, it, vi } from 'vitest'
import Status from '../src/components/Status.vue'
import type { NetworkInstance } from '../src/types/network'

vi.mock('vue-i18n', () => ({ useI18n: () => ({
  t: (key: string, params?: Record<string, unknown>) => params ? `${key} ${JSON.stringify(params)}` : key,
}) }))

const wrappers: VueWrapper[] = []

afterEach(() => {
  wrappers.splice(0).forEach(wrapper => wrapper.unmount())
  vi.useRealTimers()
  vi.restoreAllMocks()
})

async function render(localStun: unknown, peerStun: unknown, lossRate?: number, vpnStatus?: unknown,
  apiOverrides: Record<string, unknown> = {}) {
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
    attachTo: document.body,
    props: {
      curNetworkInst: instance,
      api: {
        get_network_config: vi.fn(async () => ({})),
        get_vpn_status: vi.fn(async () => vpnStatus),
        ...apiOverrides,
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

function vpnResponse(overrides: Record<string, unknown> = {}) {
  return { enabled: true, exit_enabled: true, rules: [
    { cidr: '203.0.113.0/24', exit_addr: '10.9.0.2', state: 'down' },
    { cidr: '198.51.100.0/24', exit_addr: '10.9.0.3', state: 'up' },
  ], routes_active: true, routes_installed: 2, capture_routes: [], policy_generation: 1n, ipv6_addr: '', ...overrides }
}

async function expandNodeDetails(wrapper: VueWrapper) {
  await wrapper.findAll('button').find(button => button.text() === 'show_node_details')!.trigger('click')
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

  it('shows compact VPN status and each single exit only inside expanded node details', async () => {
    const wrapper = await render(undefined, undefined, undefined, vpnResponse())
    const panel = wrapper.find('.node-details .vpn-status')
    expect(wrapper.findAll('.p-card')).toHaveLength(2)
    expect(panel.exists()).toBe(true)
    expect(panel.isVisible()).toBe(false)
    const toggle = wrapper.find('button[aria-controls]')
    expect(toggle.attributes('aria-controls')).toBe(wrapper.find('.node-details').attributes('id'))
    expect(toggle.attributes('aria-expanded')).toBe('false')
    await expandNodeDetails(wrapper)
    expect(panel.isVisible()).toBe(true)
    expect(toggle.attributes('aria-expanded')).toBe('true')
    expect(panel.text()).toContain('vpn_status_enabled')
    expect(panel.text()).toContain('vpn_status_routes_active')
    expect(panel.text()).toContain('vpn_status_routes_count {"count":2}')
    expect(panel.findAll('.vpn-rule-row')).toHaveLength(2)
    const text = wrapper.text()
    for (const value of ['203.0.113.0/24', '198.51.100.0/24', '10.9.0.2', '10.9.0.3', 'vpn_exit_state_down', 'vpn_exit_state_up']) {
      expect(text).toContain(value)
    }
    for (const value of ['vpn_status_exit_offering', 'vpn_rule_effective', 'vpn_ignore_private', 'vpn_rule_packets', 'vpn_rule_priority']) {
      expect(text).not.toContain(value)
    }
    await wrapper.findAll('button').find(button => button.text() === 'hide_node_details')!.trigger('click')
    expect(panel.isVisible()).toBe(false)
  })

  it('shows disabled VPN, inactive system routes and the empty rule state in node details', async () => {
    const wrapper = await render(undefined, undefined, undefined, vpnResponse({ enabled: false,
      exit_enabled: false, routes_active: false, routes_installed: 0, rules: [] }))
    await expandNodeDetails(wrapper)
    const panel = wrapper.find('.vpn-status')
    expect(panel.text()).toContain('vpn_status_disabled')
    expect(panel.text()).toContain('vpn_status_routes_inactive')
    expect(panel.text()).toContain('vpn_status_routes_count {"count":0}')
    expect(panel.text()).toContain('vpn_no_rules')
    expect(panel.find('.vpn-rule-list').exists()).toBe(false)
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

    const alert = wrapper.find('.node-details [role="alert"]')
    expect(alert.exists()).toBe(true)
    expect(alert.isVisible()).toBe(false)
    await expandNodeDetails(wrapper)
    expect(alert.isVisible()).toBe(true)
    expect(alert.text()).toBe(error)
  })

  it.each(['success', 'failure'] as const)('ignores an old node VPN response (%s) after switching nodes', async outcome => {
    let finish!: () => void
    const nextStatus = vpnResponse({ rules: [{ cidr: '192.0.2.0/24', exit_addr: '10.9.0.4', state: 'up' }] })
    const getVpnStatus = vi.fn().mockImplementationOnce(() => new Promise((resolve, reject) => {
      finish = () => outcome === 'success' ? resolve(vpnResponse()) : reject(new Error('old node offline'))
    })).mockResolvedValue(nextStatus)
    const wrapper = await render(undefined, undefined, undefined, undefined, { get_vpn_status: getVpnStatus })
    await expandNodeDetails(wrapper)
    await wrapper.setProps({ curNetworkInst: { ...wrapper.props('curNetworkInst')!,
      instance_id: '12345678-9abc-def0-fedc-ba9876543211' } })
    await flushPromises()
    expect(wrapper.find('.vpn-status').text()).toContain('192.0.2.0/24')
    finish()
    await flushPromises()
    expect(wrapper.find('.vpn-status').text()).toContain('192.0.2.0/24')
    expect(wrapper.find('.vpn-status').text()).not.toContain('203.0.113.0/24')
  })

  it('clears VPN status on connection replacement and stop, and does not refill it after unmount', async () => {
    vi.useFakeTimers()
    const wrapper = await render(undefined, undefined, undefined, vpnResponse())
    const nextGet = vi.fn().mockResolvedValue(vpnResponse({ enabled: false }))
    await wrapper.setProps({ api: { ...wrapper.props('api'), scope: 'connection-b', get_vpn_status: nextGet } })
    await flushPromises()
    expect(wrapper.find('.vpn-status').text()).toContain('vpn_status_disabled')
    await wrapper.setProps({ curNetworkInst: { ...wrapper.props('curNetworkInst')!, running: false } })
    expect(wrapper.find('.vpn-status').exists()).toBe(false)
    await vi.advanceTimersByTimeAsync(4_000)
    expect(nextGet).toHaveBeenCalledOnce()

    let finish!: (response: any) => void
    nextGet.mockImplementationOnce(() => new Promise(resolve => { finish = resolve }))
    await wrapper.setProps({ curNetworkInst: { ...wrapper.props('curNetworkInst')!, running: true } })
    await flushPromises()
    wrapper.unmount()
    finish(vpnResponse())
    await flushPromises()
    await vi.advanceTimersByTimeAsync(4_000)
    expect(nextGet).toHaveBeenCalledTimes(2)
  })

  it('clears failed VPN observations, retries them serially and recovers after reconnecting', async () => {
    vi.useFakeTimers()
    vi.spyOn(console, 'warn').mockImplementation(() => {})
    const getVpnStatus = vi.fn().mockResolvedValueOnce(vpnResponse())
      .mockRejectedValueOnce(new Error('node offline')).mockResolvedValue(vpnResponse())
    const wrapper = await render(undefined, undefined, undefined, undefined, { get_vpn_status: getVpnStatus })
    await vi.advanceTimersByTimeAsync(2_000)
    expect(wrapper.find('.vpn-status').exists()).toBe(false)
    await vi.advanceTimersByTimeAsync(2_000)
    expect(wrapper.find('.vpn-status').exists()).toBe(true)
    expect(getVpnStatus).toHaveBeenCalledTimes(3)
  })
})
