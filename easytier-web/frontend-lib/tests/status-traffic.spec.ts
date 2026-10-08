import { mount } from '@vue/test-utils'
import PrimeVue from 'primevue/config'
import { describe, expect, it, vi } from 'vitest'
import { defineComponent, h } from 'vue'
import Status from '../src/components/Status.vue'
import type { NetworkInstance } from '../src/types/network'

vi.mock('vue-i18n', () => ({
  useI18n: () => ({ t: (key: string) => key }),
}))

vi.mock('@vueuse/core', () => ({
  useTimeAgo: () => '',
}))

vi.mock('../src/components/NetworkChart.vue', () => ({
  default: defineComponent({ render: () => h('div') }),
}))

function runningInstance(): NetworkInstance {
  return {
    instance_id: '12345678-9abc-def0-fedc-ba9876543210',
    running: true,
    error_msg: '',
    detail: {
      my_node_info: { hostname: 'local', version: 'test' },
      peer_route_pairs: [{
        route: { hostname: 'peer', version: 'test', cost: 1 },
        peer: {
          conns: [{
            conn_id: 'live',
            stats: {
              tx_bytes: '2048',
              rx_bytes: '0',
              bandwidth_estimate_version: 1,
              estimated_tx_bps: 1_000_000,
              estimated_rx_bps: 2_000_000,
            },
          }],
        },
      }],
    },
  } as unknown as NetworkInstance
}

describe('Status traffic columns', () => {
  it('combines total traffic into two rows and keeps valid estimates across timer ticks', async () => {
    vi.useFakeTimers()
    const wrapper = mount(Status, {
      props: {
        curNetworkInst: runningInstance(),
        api: { get_network_config: vi.fn(async () => ({})) } as any,
      },
      global: {
        plugins: [PrimeVue],
        directives: { tooltip: () => {} },
        stubs: { HumanEvent: true },
      },
    })

    try {
      const headers = wrapper.findAll('th').map(header => header.text())
      expect(headers).toContain('total_traffic')
      expect(headers).not.toContain('upload_bytes')
      expect(headers).not.toContain('download_bytes')
      expect(headers.indexOf('estimated_bandwidth')).toBe(headers.indexOf('total_traffic') + 1)

      const peerRow = wrapper.findAll('tbody tr')[1]
      const cells = peerRow.findAll('td')
      const trafficCell = cells[headers.indexOf('total_traffic')]
      expect(trafficCell.findAll('div.whitespace-nowrap > div').map(line => line.text())).toEqual([
        'upload: 2.0 KiB',
        'download: 0 B',
      ])
      expect(wrapper.findAll('tbody tr')[0].text()).toContain('upload: --')

      await vi.advanceTimersByTimeAsync(10_000)
      expect(peerRow.text()).toContain('upload: 1.00 Mbit/s')
      expect(peerRow.text()).toContain('download: 2.00 Mbit/s')

      const refreshed = runningInstance()
      refreshed.detail!.peer_route_pairs![0].peer!.conns[0].stats!.estimated_tx_bps = 3_000_000
      await wrapper.setProps({ curNetworkInst: refreshed })
      expect(peerRow.text()).toContain('upload: 3.00 Mbit/s')
    } finally {
      wrapper.unmount()
      vi.useRealTimers()
    }
  })
})
