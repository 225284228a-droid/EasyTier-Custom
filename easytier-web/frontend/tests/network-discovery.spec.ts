import { describe, expect, it } from 'vitest'
import { NetworkTypes } from 'easytier-frontend-lib'
import { groupNetworks, mergeConfigFields, variedFields, type DeviceNetworkScan } from '../src/modules/networkDiscovery'
import { buildTopology, networkNameIdentity } from '../src/modules/networkTopology'

const device = (id: string) => ({ machine_id: id, hostname: id, running_network_count: 0 }) as any

const scan = (id: string, running: Record<string, string>, disabled: Record<string, string> = {}): DeviceNetworkScan => ({
  device: device(id),
  running: Object.keys(running),
  disabled: Object.keys(disabled),
  metas: Object.fromEntries(Object.entries({ ...running, ...disabled })
    .map(([instance, name]) => [instance, { network_name: name, config_permission: 0 }])),
})

const config = (patch: Partial<NetworkTypes.NetworkConfig>) => ({ ...NetworkTypes.DEFAULT_NETWORK_CONFIG(), ...patch })

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

  it('skips configurations without a network name', () => {
    expect(groupNetworks([scan('a', { i1: ' ' })])).toEqual([])
  })
})

describe('batch merge', () => {
  it('copies only the edited fields and keeps per-device values', () => {
    const target = config({ hostname: 'node-a', virtual_ipv4: '10.0.0.2', dhcp: false, network_secret: 'old', peer_urls: ['tcp://1.1.1.1:11010'] })
    const source = config({ hostname: 'node-b', virtual_ipv4: '10.0.0.3', dhcp: false, network_secret: 'new', peer_urls: ['tcp://2.2.2.2:11010'] })
    const merged = mergeConfigFields(target, source, ['network_secret', 'peer_urls'])
    expect(merged.instance_id).toBe(target.instance_id)
    expect(merged.network_secret).toBe('new')
    expect(merged.peer_urls).toEqual(['tcp://2.2.2.2:11010'])
    expect(merged.hostname).toBe('node-a')
    expect(merged.virtual_ipv4).toBe('10.0.0.2')
  })

  it('treats a value cleared in the template as a reset', () => {
    const target = config({ peer_urls: ['tcp://1.1.1.1:11010'], latency_first: true })
    const source = config({ peer_urls: [], latency_first: false })
    const merged = mergeConfigFields(target, source, ['peer_urls', 'latency_first'])
    expect(merged.peer_urls).toEqual([])
    expect(merged.latency_first).toBe(false)
  })

  it('shares the secure mode switch but not the node key', () => {
    const target = config({ network_secret: 's', secure_mode: { enabled: false, local_private_key: 'key-a' } as any })
    const source = config({ network_secret: 's', secure_mode: { enabled: true, local_private_key: 'key-b' } as any })
    const merged = mergeConfigFields(target, source, ['secure_mode'])
    expect(merged.secure_mode?.enabled).toBe(true)
    expect(merged.secure_mode?.local_private_key).toBe('key-a')
  })

  it('lists fields that differ between members', () => {
    expect(variedFields([
      config({ hostname: 'a', network_secret: 's' }),
      config({ hostname: 'b', network_secret: 's' }),
    ])).toEqual(['hostname'])
    expect(variedFields([config({})])).toEqual([])
    expect(variedFields([
      config({ peer_urls: ['tcp://1.1.1.1:11010'] }),
      config({ peer_urls: [] }),
    ])).toEqual(['peer_urls'])
  })
})
