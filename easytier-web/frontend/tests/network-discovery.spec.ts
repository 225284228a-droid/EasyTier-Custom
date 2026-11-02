import { describe, expect, it } from 'vitest'
import { groupNetworks, type DeviceNetworkScan } from '../src/modules/networkDiscovery'
import { ADVANCED_SWITCHES, advancedDistribution, touchedSettings } from '../src/modules/meshAdvanced'
import { buildTopology, networkNameIdentity } from '../src/modules/networkTopology'

const device = (id: string) => ({ machine_id: id, hostname: id, running_network_count: 0 }) as any

const scan = (id: string, running: Record<string, string>, disabled: Record<string, string> = {}): DeviceNetworkScan => ({
  device: device(id),
  running: Object.keys(running),
  disabled: Object.keys(disabled),
  metas: Object.fromEntries(Object.entries({ ...running, ...disabled })
    .map(([instance, name]) => [instance, { network_name: name, config_permission: 0 }])),
})


describe('network discovery', () => {
  it('groups device configurations by network name, running or stopped', () => {
    const networks = groupNetworks([
      scan('b', { i2: 'office' }, { i3: 'lab' }),
      scan('a', { i1: 'office' }),
    ])
    expect(networks.map(network => network.name)).toEqual(['lab', 'office'])
    const office = networks[1]
    expect(office.members.map(member => member.key)).toEqual(['a/i1', 'b/i2'])
    expect(office.runningCount).toBe(2)
    expect(networks[0].runningCount).toBe(0)
  })

  it('uses the topology identity for a network name', () => {
    const [network] = groupNetworks([scan('a', { i1: 'office' })])
    const topology = buildTopology([], [{
      device: device('a'),
      instanceId: 'i1',
      detail: { running: true, network_name: 'office', my_node_info: { peer_id: 7 } } as any,
    }])
    expect(network.identity).toBe(networkNameIdentity('office'))
    expect(topology.nodes[0].networkIdentity).toBe(network.identity)
  })

  it('keeps a central network visible without online members', () => {
    const networks = groupNetworks([], [{ network_id: 'n1', network_name: 'central-mesh', display_name: 'HQ' } as any])
    expect(networks).toHaveLength(1)
    expect(networks[0].central?.network_id).toBe('n1')
    expect(networks[0].members).toEqual([])
  })

  it('labels a network by its display name but not by placeholder instance names', () => {
    const named = (id: string, instance: string, instanceName: string): DeviceNetworkScan => {
      const result = scan(id, { [instance]: 'office' })
      result.metas[instance] = { ...result.metas[instance], instance_name: instanceName }
      return result
    }
    expect(groupNetworks([named('a', 'i1', 'Head office')])[0].label).toBe('Head office')
    expect(groupNetworks([named('a', 'i1', 'i1'), named('b', 'i2', 'default')])[0].label).toBe('office')
  })

  it('keeps a network the console records before its first member', () => {
    const networks = groupNetworks([], [], [], [], [{
      network_name: 'lab',
      core: { network_name: 'lab', network_secret: 's', secure_mode: false, disable_encryption: false, display_name: 'Lab' },
      preset: { peers: [], listeners: [], advanced: {} },
      auto_join: true,
    }])
    expect(networks.map(network => network.name)).toEqual(['lab'])
    expect(networks[0].label).toBe('Lab')
    expect(networks[0].record?.auto_join).toBe(true)
  })

  it('marks locked members', () => {
    const [network] = groupNetworks([scan('a', { i1: 'office' })], [], [{
      device_id: 'a', instance_id: 'i1', network_name: 'office', online: true, pending: false, credential: false, locked: true,
    }])
    expect(network.members[0].locked).toBe(true)
  })

  it('skips configurations without a network name', () => {
    expect(groupNetworks([scan('a', { i1: ' ' })])).toEqual([])
  })
})

describe('advanced settings', () => {
  it('counts how members hold each setting', () => {
    const distribution = advancedDistribution([
      { latency_first: true, mtu: 1300 },
      { latency_first: null, mtu: 1300 },
      {},
    ])
    expect(Object.fromEntries(distribution.latency_first)).toEqual({ true: 1, default: 2 })
    expect(Object.fromEntries(distribution.mtu)).toEqual({ 1300: 2, default: 1 })
  })

  it('sends only touched settings, a reset included', () => {
    expect(touchedSettings({ latency_first: true, mtu: undefined, lazy_p2p: null }))
      .toEqual({ latency_first: true, lazy_p2p: null })
  })

  it('never offers device settings, private mode or encryption', () => {
    for (const field of ['enable_exit_node', 'no_tun', 'use_smoltcp', 'bind_device', 'proxy_forward_by_system', 'enable_private_mode', 'disable_encryption']) {
      expect(ADVANCED_SWITCHES).not.toContain(field)
    }
  })
})
