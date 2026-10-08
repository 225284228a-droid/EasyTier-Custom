import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { flushPromises, mount } from '@vue/test-utils'
import { createI18n } from 'vue-i18n'
import Dashboard from '../src/components/Dashboard.vue'

const machineId = '00000000-0000-0000-0000-000000000001'
const instanceId = '00000000-0000-0000-0000-00000000000b'
const uuid = (id: number) => ({ part1: 0, part2: 0, part3: 0, part4: id })
const devices = [{
  info: { machine_id: uuid(1), hostname: 'node-a', running_network_instances: [uuid(11)] },
}]
const info = {
  [instanceId]: {
    running: true,
    network_name: 'mesh',
    my_node_info: { peer_id: 1, hostname: 'node-a' },
    node_location: { country: 'China', city: 'Shanghai', public_ip: '1.1.1.1', latitude: 31.2, longitude: 121.5 },
    peers: [],
    routes: [],
  },
}
const globe = { props: ['nodes', 'links', 'loading', 'persistenceKey'], template: '<div />' }
const api = () => ({
  persistenceScope: 'https://dashboard-test.invalid',
  get_summary: vi.fn().mockResolvedValue({ device_count: devices.length }),
  list_machines: vi.fn().mockResolvedValue(devices),
  collect_machine_network_info: vi.fn().mockResolvedValue(info),
})
const wrappers: ReturnType<typeof mount>[] = []

async function dashboard(client: ReturnType<typeof api>) {
  const wrapper = mount(Dashboard, {
    props: { api: client as any },
    global: {
      plugins: [createI18n({ legacy: false, locale: 'en', missingWarn: false, fallbackWarn: false })],
      stubs: { NetworkTopologyGlobe: globe },
    },
  })
  wrappers.push(wrapper)
  await flushPromises()
  return wrapper
}

describe('dashboard persistent display and warning grace', () => {
  beforeEach(() => {
    localStorage.clear()
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date', 'performance'] })
    vi.setSystemTime(new Date('2026-10-06T00:00:00Z'))
    vi.spyOn(console, 'warn').mockImplementation(() => {})
  })

  afterEach(() => {
    for (const wrapper of wrappers.splice(0))
      wrapper.unmount()
    vi.useRealTimers()
    vi.restoreAllMocks()
    localStorage.clear()
  })

  it('suppresses short failures, warns after sixty seconds, and clears on recovery', async () => {
    const client = api()
    const wrapper = await dashboard(client)
    client.collect_machine_network_info.mockRejectedValue(new Error('temporary'))
    await vi.advanceTimersByTimeAsync(59_999)
    expect(wrapper.find('.dashboard-error').exists()).toBe(false)
    await vi.advanceTimersByTimeAsync(1)
    expect(wrapper.find('.dashboard-error').text()).toBe('web.dashboard.partial_failure')
    expect(wrapper.findComponent(globe).props('nodes')).toHaveLength(1)
    client.collect_machine_network_info.mockResolvedValue(info)
    await vi.advanceTimersByTimeAsync(4_000)
    expect(wrapper.find('.dashboard-error').exists()).toBe(false)
  })

  it('restores last known nodes after authorization while awaiting a fresh report', async () => {
    const first = await dashboard(api())
    expect(first.findComponent(globe).props('nodes')).toHaveLength(1)
    first.unmount()
    let finish!: (value: typeof info) => void
    const client = api()
    client.collect_machine_network_info.mockImplementation(() => new Promise(resolve => { finish = resolve }))
    const reopened = await dashboard(client)
    expect(reopened.findComponent(globe).props('nodes')[0]).toMatchObject({
      machineId, label: 'node-a', stale: true,
    })
    reopened.unmount()
    finish(info)
    await flushPromises()
  })

  it('does not display archived nodes to a different authorized device list', async () => {
    const first = await dashboard(api())
    first.unmount()
    const client = api()
    client.list_machines.mockResolvedValue([])
    const reopened = await dashboard(client)
    expect(reopened.findComponent(globe).props('nodes')).toEqual([])
  })

  it('keeps initial collection and automatic polling silent, including pending requests', async () => {
    let finish!: (value: typeof info) => void
    const client = api()
    client.collect_machine_network_info.mockImplementation(() => new Promise(resolve => { finish = resolve }))
    const wrapper = await dashboard(client)
    expect(wrapper.findComponent(globe).props('loading')).toBe(false)
    finish(info)
    await flushPromises()

    await vi.advanceTimersByTimeAsync(2_000)
    expect(client.collect_machine_network_info).toHaveBeenCalledTimes(2)
    expect(wrapper.findComponent(globe).props('loading')).toBe(false)
    expect(wrapper.findComponent(globe).props('nodes')).toHaveLength(1)
    wrapper.findComponent(globe).vm.$emit('refresh')
    await flushPromises()
    expect(wrapper.findComponent(globe).props('loading')).toBe(true)
    expect(client.collect_machine_network_info).toHaveBeenCalledTimes(2)
    finish(info)
    await flushPromises()
    expect(wrapper.findComponent(globe).props('loading')).toBe(false)
  })

  it('shows a loading state only for a requested manual refresh and clears it on completion', async () => {
    const client = api()
    const wrapper = await dashboard(client)
    let finish!: (value: typeof info) => void
    client.collect_machine_network_info.mockImplementation(() => new Promise(resolve => { finish = resolve }))
    wrapper.findComponent(globe).vm.$emit('refresh')
    await flushPromises()
    expect(client.collect_machine_network_info).toHaveBeenCalledTimes(2)
    expect(wrapper.findComponent(globe).props('loading')).toBe(true)
    finish(info)
    await flushPromises()
    expect(wrapper.findComponent(globe).props('loading')).toBe(false)
  })
})
