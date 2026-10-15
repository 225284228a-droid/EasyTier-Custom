import { beforeEach, describe, expect, it, vi } from 'vitest'
import { IPv4CidrRange, IPv6CidrRange } from 'ip-num/IPRange'
import type { MobileTunConfig } from './backend'

const mocks = vi.hoisted(() => {
  const listeners = new Map<string, (payload: unknown) => Promise<void>>()
  const configs = new Map<string, Record<string, unknown>>()
  const networkInfo = new Map<string, unknown>()

  return {
    listeners,
    configs,
    networkInfo,
    addPluginListener: vi.fn(async (_plugin: string, event: string, listener: (payload: unknown) => Promise<void>) => {
      listeners.set(event, listener)
    }),
    collectNetworkInfo: vi.fn(async (instanceId: string) => ({
      info: { map: { [instanceId]: networkInfo.get(instanceId) } },
    })),
    consumeVpnTileAction: vi.fn(async () => ({})),
    completeVpnReconfigure: vi.fn(async () => ({ accepted: true })),
    getUnderlyingDnsServers: vi.fn(async () => ({ dnsServers: ['192.0.2.53'], systemDnsServers: ['192.0.2.53'] })),
    getCoreVpnStatus: vi.fn(async (_id: string) => ({ policy_generation: '1' })),
    getConfig: vi.fn(async (instanceId: string) => configs.get(instanceId)),
    getVpnStatus: vi.fn<() => Promise<Record<string, unknown>>>(async () => ({ running: false })),
    listNetworkInstanceIds: vi.fn<() => Promise<{ running_inst_ids: unknown[] }>>(async () => ({ running_inst_ids: [] })),
    prepareVpn: vi.fn(async () => ({ granted: true })),
    getMobileTunConfig: vi.fn(async (instanceIds: string[]) => makeMobileTunConfig(instanceIds)),
    setTunFd: vi.fn(async (fd: number, instanceIds?: string[]) => fd > 0
      ? makeMobileTunConfig(instanceIds ?? [...configs.keys()])
      : { instance_ids: [], addresses: [], routes: [], rejected: [] }),
    startVpn: vi.fn(async (request: any) => {
      await listeners.get('vpn_service_start')?.({ fd: 1, generation: request.generation, instanceIds: request.instanceIds })
      return {}
    }),
    stopVpn: vi.fn(async () => {
      await listeners.get('vpn_service_stop')?.({})
      return {}
    }),
  }
})

vi.mock('@tauri-apps/api/core', () => ({
  addPluginListener: mocks.addPluginListener,
}))

vi.mock('easytier-frontend-lib', () => ({
  Utils: {
    UuidToStr: (value: unknown) => String(value),
    ipv4ToString: (address: { addr: string }) => address.addr,
  },
}))

vi.mock('tauri-plugin-vpnservice-api', () => ({
  complete_vpn_reconfigure: mocks.completeVpnReconfigure,
  get_underlying_dns_servers: mocks.getUnderlyingDnsServers,
  reconfigure_vpn: mocks.startVpn,
  consume_vpn_tile_action: mocks.consumeVpnTileAction,
  get_vpn_status: mocks.getVpnStatus,
  prepare_vpn: mocks.prepareVpn,
  start_vpn: mocks.startVpn,
  stop_vpn: mocks.stopVpn,
}))

vi.mock('./backend', () => ({
  collectNetworkInfo: mocks.collectNetworkInfo,
  getConfig: mocks.getConfig,
  getMobileTunConfig: mocks.getMobileTunConfig,
  getVpnStatus: mocks.getCoreVpnStatus,
  listNetworkInstanceIds: mocks.listNetworkInstanceIds,
  setTunFd: mocks.setTunFd,
}))

function makeMobileTunConfig(instanceIds: string[]): MobileTunConfig {
  const addresses: string[] = []
  const routes = new Set<string>()
  let dns: string | undefined
  for (const id of instanceIds) {
    const config = mocks.configs.get(id)!
    const info = mocks.networkInfo.get(id) as any
    const ipv4 = info?.my_node_info?.virtual_ipv4
    const ipv6 = info?.my_node_info?.virtual_ipv6
    if ((!ipv4?.address?.addr && !ipv6?.address?.addr) || info.error_msg)
      continue
    if (ipv4?.address?.addr) {
      const address = `${ipv4.address.addr}/${ipv4.network_length || 24}`
      addresses.push(address)
      const range = IPv4CidrRange.fromCidr(address)
      routes.add(`${range.getFirst()}/${range.getPrefix()}`)
    }
    if (ipv6?.address?.addr) {
      const address = `${ipv6.address.addr}/${ipv6.network_length || 64}`
      addresses.push(address)
      const range = IPv6CidrRange.fromCidr(address)
      routes.add(`${range.getFirst()}/${range.getPrefix()}`)
    }
    const configuredRoutes = config.enable_manual_routes
      ? config.routes as string[]
      : (info.routes ?? []).flatMap((route: any) => route.proxy_cidrs ?? [])
    configuredRoutes.forEach((route: string) => routes.add(route.includes('/') ? route : `${route}/32`))
    if (config.enable_magic_dns && ipv4?.address?.addr) {
      routes.add('100.100.100.101/32')
      dns = '100.100.100.101'
    }
  }
  return { instance_ids: [...instanceIds].sort(), addresses: addresses.sort(), routes: [...routes].sort(), dns, rejected: [] }
}

function setConfig(instanceId: string, noTun = false, devName?: string) {
  mocks.configs.set(instanceId, {
    no_tun: noTun,
    dev_name: devName,
    dhcp: false,
    enable_magic_dns: false,
    routes: [],
  })
  mocks.listNetworkInstanceIds.mockResolvedValue({ running_inst_ids: [...mocks.configs.keys()] })
}

function setReady(instanceId: string, ipv4: string) {
  mocks.networkInfo.set(instanceId, {
    my_node_info: {
      virtual_ipv4: {
        address: { addr: ipv4 },
        network_length: 24,
      },
    },
    routes: [],
  })
}

function setReadyIpv6(instanceId: string, ipv6: string) {
  const info = mocks.networkInfo.get(instanceId) as any ?? { my_node_info: {}, routes: [] }
  info.my_node_info.virtual_ipv6 = { address: { addr: ipv6 }, network_length: 64 }
  mocks.networkInfo.set(instanceId, info)
}

async function loadVpnModule() {
  const mobileVpn = await import('./mobile_vpn')
  await mobileVpn.initMobileVpnService()
  return mobileVpn
}

beforeEach(() => {
  vi.useFakeTimers()
  vi.clearAllTimers()
  vi.resetModules()
  mocks.listeners.clear()
  mocks.configs.clear()
  mocks.networkInfo.clear()
  mocks.addPluginListener.mockClear()
  mocks.collectNetworkInfo.mockClear()
  mocks.consumeVpnTileAction.mockReset()
  mocks.consumeVpnTileAction.mockResolvedValue({})
  mocks.getConfig.mockClear()
  mocks.getVpnStatus.mockReset()
  mocks.getVpnStatus.mockResolvedValue({ running: false })
  mocks.listNetworkInstanceIds.mockReset()
  mocks.listNetworkInstanceIds.mockResolvedValue({ running_inst_ids: [] })
  mocks.prepareVpn.mockClear()
  mocks.completeVpnReconfigure.mockReset()
  mocks.completeVpnReconfigure.mockResolvedValue({ accepted: true })
  mocks.getUnderlyingDnsServers.mockReset()
  mocks.getUnderlyingDnsServers.mockResolvedValue({ dnsServers: ['192.0.2.53'], systemDnsServers: ['192.0.2.53'] })
  mocks.getCoreVpnStatus.mockReset()
  mocks.getCoreVpnStatus.mockResolvedValue({ policy_generation: '1' })
  mocks.setTunFd.mockClear()
  mocks.setTunFd.mockImplementation(async (fd: number, instanceIds?: string[]) => fd > 0
    ? makeMobileTunConfig(instanceIds ?? [...mocks.configs.keys()])
    : { instance_ids: [], addresses: [], routes: [], rejected: [] })
  mocks.getMobileTunConfig.mockReset()
  mocks.getMobileTunConfig.mockImplementation(async instanceIds => makeMobileTunConfig(instanceIds))
  mocks.startVpn.mockClear()
  mocks.stopVpn.mockClear()
})

describe('mobile VPN reconciliation', () => {
  it('starts an IPv6-only VPN after DHCPv6 becomes ready', async () => {
    setConfig('A')
    mocks.configs.get('A')!.dhcpv6 = true
    mocks.configs.get('A')!.enable_magic_dns = true
    mocks.networkInfo.set('A', { my_node_info: {}, routes: [] })
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    expect(mocks.startVpn).not.toHaveBeenCalled()

    setReadyIpv6('A', 'fd12::7')
    await vi.advanceTimersByTimeAsync(2000)
    expect(mocks.startVpn).toHaveBeenCalledOnce()
    expect(mocks.startVpn).toHaveBeenCalledWith(expect.objectContaining({
      ipv4Addrs: ['fd12::7/64'],
      routes: ['fd12:0:0:0:0:0:0:0/64'],
      dnsServers: ['192.0.2.53'],
    }))
    expect(mocks.setTunFd).toHaveBeenCalledWith(1, ['A'], expect.any(String))
  })

  it('preserves IPv4 when the DHCPv6 address changes or is withdrawn', async () => {
    setConfig('A')
    mocks.configs.get('A')!.dhcpv6 = true
    setReady('A', '10.0.0.1')
    setReadyIpv6('A', 'fd12::7')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')

    setReadyIpv6('A', 'fd13::8')
    await vpn.onNetworkInstanceUpdate('A')
    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).toHaveBeenLastCalledWith(expect.objectContaining({
      ipv4Addrs: ['10.0.0.1/24', 'fd13::8/64'],
      routes: ['10.0.0.0/24', 'fd13:0:0:0:0:0:0:0/64'],
    }))

    setReady('A', '10.0.0.1')
    await vpn.onNetworkInstanceUpdate('A')
    expect(mocks.startVpn).toHaveBeenLastCalledWith(expect.objectContaining({
      ipv4Addrs: ['10.0.0.1/24'],
      routes: ['10.0.0.0/24'],
    }))
  })

  it('keeps IPv6 active after IPv4 is withdrawn and stops after both are lost', async () => {
    setConfig('A')
    mocks.configs.get('A')!.dhcp = true
    mocks.configs.get('A')!.dhcpv6 = true
    setReady('A', '10.0.0.1')
    setReadyIpv6('A', 'fd12::7')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')

    mocks.networkInfo.set('A', { my_node_info: {}, routes: [] })
    setReadyIpv6('A', 'fd12::7')
    await vpn.onNetworkInstanceUpdate('A')
    expect(mocks.startVpn).toHaveBeenLastCalledWith(expect.objectContaining({
      ipv4Addrs: ['fd12::7/64'],
    }))
    expect(mocks.setTunFd).toHaveBeenLastCalledWith(1, ['A'], expect.any(String))

    mocks.networkInfo.set('A', { my_node_info: {}, routes: [] })
    await vpn.onNetworkInstanceUpdate('A')
    expect(mocks.setTunFd).toHaveBeenLastCalledWith(0, undefined, expect.any(String))
  })

  it('adds a DHCPv6 member to the shared VPN without blocking a ready IPv4 member', async () => {
    setConfig('A')
    setConfig('B')
    setReady('A', '10.0.0.1')
    mocks.configs.get('B')!.dhcpv6 = true
    mocks.networkInfo.set('B', { my_node_info: {}, routes: [] })
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    expect(mocks.startVpn).toHaveBeenCalledWith(expect.objectContaining({
      ipv4Addrs: ['10.0.0.1/24'],
    }))

    setReadyIpv6('B', 'fd12::7')
    await vpn.onNetworkInstanceUpdate('B')
    expect(mocks.startVpn).toHaveBeenLastCalledWith(expect.objectContaining({
      ipv4Addrs: ['10.0.0.1/24', 'fd12::7/64'],
    }))
    expect(mocks.setTunFd).toHaveBeenLastCalledWith(1, ['A', 'B'], expect.any(String))
  })

  it('preserves healthy members and reports a rejected bootstrap member', async () => {
    setConfig('A')
    setReady('A', '10.126.126.1')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    const errorHandler = vi.fn()
    vpn.setMobileVpnErrorHandler(errorHandler)
    mocks.startVpn.mockClear()
    mocks.setTunFd.mockClear()
    setConfig('B')
    setReady('B', '10.126.126.2')
    mocks.getMobileTunConfig.mockResolvedValue({ ...makeMobileTunConfig(['A']), rejected: [{ instance_id: 'B', error: 'overlapping subnet' }] })
    await vpn.onNetworkInstanceChange('B')
    await vpn.onNetworkInstanceUpdate('B')
    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).not.toHaveBeenCalled()
    expect(mocks.setTunFd).not.toHaveBeenCalled()
    expect(errorHandler).toHaveBeenCalledOnce()
    expect(errorHandler).toHaveBeenCalledWith('B: overlapping subnet')
  })

  it('does not invalidate healthy attachments on a collection notification', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    mocks.setTunFd.mockClear()
    await vpn.onNetworkInstancesChange()
    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).toHaveBeenCalledOnce()
    expect(mocks.setTunFd).not.toHaveBeenCalled()
  })

  it('corrects OS configuration when attachment admits fewer members', async () => {
    setConfig('A')
    setConfig('B')
    setReady('A', '10.0.0.1')
    setReady('B', '10.0.1.1')
    const actual = { ...makeMobileTunConfig(['A']), rejected: [{ instance_id: 'B', error: 'claim changed during attachment' }] }
    mocks.setTunFd.mockResolvedValue(actual)
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    expect(mocks.stopVpn).not.toHaveBeenCalled()
    mocks.getMobileTunConfig.mockResolvedValue(actual)
    await vi.advanceTimersByTimeAsync(2000)
    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).toHaveBeenLastCalledWith(expect.objectContaining({ ipv4Addrs: ['10.0.0.1/24'], routes: ['10.0.0.0/24'] }))
  })

  it('waits for native attachment completion before completing startup', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    let resolve!: (value: ReturnType<typeof makeMobileTunConfig>) => void
    mocks.setTunFd.mockImplementationOnce(() => new Promise(r => { resolve = r }))
    const vpn = await loadVpnModule()
    let completed = false
    const start = vpn.onNetworkInstanceChange('A').then(() => { completed = true })
    await vi.waitFor(() => expect(mocks.setTunFd).toHaveBeenCalledWith(1, ['A'], expect.any(String)))
    expect(completed).toBe(false)
    resolve(makeMobileTunConfig(['A']))
    await start
    expect(completed).toBe(true)
  })

  it('retains capture and retries a physical TUN attachment failure', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    mocks.setTunFd.mockRejectedValueOnce(new Error('physical TUN failed'))
    const vpn = await loadVpnModule()
    const errorHandler = vi.fn()
    vpn.setMobileVpnErrorHandler(errorHandler)
    await vpn.onNetworkInstanceChange('A')
    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.completeVpnReconfigure).toHaveBeenCalledWith(expect.any(String), false, 'physical TUN failed')
    await vi.advanceTimersByTimeAsync(2000)
    expect(mocks.completeVpnReconfigure).toHaveBeenLastCalledWith(expect.any(String), true)
  })

  it('keeps attached shared members during a temporary status gap', async () => {
    setConfig('A', false, 'shared0')
    setConfig('B', false, 'shared0')
    setReady('A', '10.0.0.1')
    setReady('B', '10.0.1.1')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    mocks.startVpn.mockClear()

    mocks.networkInfo.delete('B')
    await vpn.onNetworkInstanceUpdate('B')
    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).not.toHaveBeenCalled()

    setReady('B', '10.0.1.2')
    await vpn.onNetworkInstanceUpdate('B')
    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).toHaveBeenLastCalledWith(expect.objectContaining({
      ipv4Addrs: ['10.0.0.1/24', '10.0.1.2/24'],
    }))
  })

  it('keeps a ready member active while a new shared member awaits an IP', async () => {
    setConfig('A', false, 'shared0')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')

    setConfig('B', false, 'shared0')
    await vpn.onNetworkInstanceChange('B')
    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).toHaveBeenCalledTimes(1)

    setReady('B', '10.0.1.1')
    await vpn.onNetworkInstanceUpdate('B')
    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).toHaveBeenLastCalledWith(expect.objectContaining({
      ipv4Addrs: ['10.0.0.1/24', '10.0.1.1/24'],
    }))
  })

  it('uses one VPN for different dev_name values and keeps the remaining member', async () => {
    setConfig('A', false, 'shared0')
    setConfig('B', false, 'other0')
    setConfig('C', true)
    setReady('A', '10.0.0.1')
    setReady('B', '10.0.1.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')
    expect(mocks.startVpn).toHaveBeenCalledWith(expect.objectContaining({
      ipv4Addrs: ['10.0.0.1/24', '10.0.1.1/24'],
    }))
    expect(mocks.setTunFd).toHaveBeenCalledWith(1, ['A', 'B'], expect.any(String))

    mocks.startVpn.mockClear()
    mocks.stopVpn.mockClear()
    mocks.listNetworkInstanceIds.mockResolvedValue({ running_inst_ids: ['C', 'B'] })
    await vpn.onNetworkInstanceChange('A')

    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).toHaveBeenCalledWith(expect.objectContaining({
      ipv4Addrs: ['10.0.1.1/24'],
    }))
    expect(mocks.setTunFd).toHaveBeenLastCalledWith(1, ['B'], expect.any(String))
  })

  it.each(['A', 'B'])('reattaches a replacement member %s with the same VPN configuration', async (replacementId) => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    mocks.setTunFd.mockClear()

    mocks.listNetworkInstanceIds.mockResolvedValue({ running_inst_ids: [] })
    const stopA = vpn.onNetworkInstanceChange('A')
    setConfig(replacementId)
    setReady(replacementId, '10.0.0.1')
    mocks.listNetworkInstanceIds.mockResolvedValue({ running_inst_ids: [replacementId] })
    const startReplacement = vpn.onNetworkInstanceChange(replacementId)
    await Promise.all([stopA, startReplacement])

    expect(mocks.setTunFd).toHaveBeenLastCalledWith(1, [replacementId], expect.any(String))
    expect(mocks.startVpn).toHaveBeenCalledTimes(2)
    expect(mocks.stopVpn).not.toHaveBeenCalled()

    mocks.setTunFd.mockClear()
    await vpn.onNetworkInstanceUpdate(replacementId)
    expect(mocks.setTunFd).not.toHaveBeenCalled()
  })

  it('does not reattach unchanged members when their order changes', async () => {
    setConfig('A')
    setConfig('B')
    setReady('A', '10.0.0.1')
    setReady('B', '10.0.1.1')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    mocks.setTunFd.mockClear()

    mocks.listNetworkInstanceIds.mockResolvedValue({ running_inst_ids: ['B', 'A'] })
    await vpn.onNetworkInstanceUpdate('A')

    expect(mocks.setTunFd).not.toHaveBeenCalled()
    expect(mocks.stopVpn).not.toHaveBeenCalled()
  })

  it('stops the VPN when only confirmed IPv4-less members remain', async () => {
    setConfig('A')
    setConfig('B')
    setReady('A', '10.0.0.1')
    mocks.networkInfo.set('B', { my_node_info: {}, routes: [] })
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')

    mocks.listNetworkInstanceIds.mockResolvedValue({ running_inst_ids: ['B'] })
    await vpn.onNetworkInstanceChange('A')

    expect(mocks.stopVpn).toHaveBeenCalledOnce()
    expect(mocks.setTunFd).toHaveBeenLastCalledWith(0, undefined, expect.any(String))
    expect(vi.getTimerCount()).toBe(0)
  })

  it('applies DHCP updates while another member has no configured IPv4', async () => {
    setConfig('A')
    setConfig('B')
    mocks.configs.get('A')!.dhcp = true
    setReady('A', '10.0.0.1')
    mocks.networkInfo.set('B', { my_node_info: {}, routes: [] })
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')

    setReady('A', '10.0.1.1')
    await vpn.onNetworkInstanceUpdate('A')

    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).toHaveBeenLastCalledWith(expect.objectContaining({
      ipv4Addrs: ['10.0.1.1/24'],
      routes: ['10.0.1.0/24'],
    }))
    expect(vi.getTimerCount()).toBe(0)
  })

  it.each([
    { virtualIpv4: '', info: { routes: [] } },
    { virtualIpv4: '10.0.1.1', info: { my_node_info: {}, routes: [] } },
  ])('waits for unconfirmed static IPv4 state: %j', async ({ virtualIpv4, info }) => {
    setConfig('A')
    setConfig('B')
    mocks.configs.get('B')!.virtual_ipv4 = virtualIpv4
    setReady('A', '10.0.0.1')
    mocks.networkInfo.set('B', info)
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')

    mocks.listNetworkInstanceIds.mockResolvedValue({ running_inst_ids: ['B'] })
    await vpn.onNetworkInstanceChange('A')

    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(vi.getTimerCount()).toBe(1)
  })

  it('removes a shared member when its DHCP address is withdrawn', async () => {
    setConfig('A', false, 'shared0')
    setConfig('B', false, 'shared0')
    mocks.configs.get('B')!.dhcp = true
    setReady('A', '10.0.0.1')
    setReady('B', '10.0.1.1')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')

    mocks.networkInfo.set('B', { my_node_info: {}, routes: [] })
    await vpn.onNetworkInstanceUpdate('B')

    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).toHaveBeenLastCalledWith(expect.objectContaining({
      ipv4Addrs: ['10.0.0.1/24'],
    }))
    expect(mocks.setTunFd).toHaveBeenLastCalledWith(1, ['A', 'B'], expect.any(String))
  })

  it('uses manual routes instead of peer proxy routes', async () => {
    setConfig('A')
    mocks.configs.get('A')!.enable_manual_routes = true
    mocks.configs.get('A')!.routes = ['192.0.2.0/24']
    setReady('A', '10.0.0.1')
    const info = mocks.networkInfo.get('A') as { routes: unknown[] }
    info.routes = [{ proxy_cidrs: ['10.9.0.0/16'] }]
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')

    expect(mocks.startVpn).toHaveBeenCalledWith(expect.objectContaining({
      routes: ['10.0.0.0/24', '192.0.2.0/24'],
    }))
  })

  it('keeps the VPN during pre-run of another instance', async () => {
    setConfig('A')
    setConfig('B')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')
    mocks.stopVpn.mockClear()

    await vpn.prepareVpnService('B')

    expect(mocks.stopVpn).not.toHaveBeenCalled()
  })

  it('preserves the VPN while retrying the same instance', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')
    mocks.stopVpn.mockClear()
    mocks.networkInfo.delete('A')

    await vpn.onNetworkInstanceUpdate('A')

    expect(mocks.stopVpn).not.toHaveBeenCalled()
  })

  it('reconciles all running members after any member update', async () => {
    setConfig('A')
    setConfig('B', false, 'other0')
    setReady('A', '10.0.0.1')
    setReady('B', '10.0.1.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')
    mocks.startVpn.mockClear()
    setReady('B', '10.0.1.2')

    await vpn.onNetworkInstanceUpdate('A')

    expect(mocks.startVpn).toHaveBeenCalledWith(expect.objectContaining({
      ipv4Addrs: ['10.0.0.1/24', '10.0.1.2/24'],
    }))
  })

  it('does not apply a stale in-flight network result', async () => {
    setConfig('A')
    setConfig('B')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()

    await vpn.onNetworkInstanceChange('A')
    mocks.startVpn.mockClear()
    mocks.stopVpn.mockClear()

    interface NetworkInfoResponse { info: { map: Record<string, unknown> } }
    let resolveNetworkInfo: (value: NetworkInfoResponse) => void = () => undefined
    let markCollectStarted: () => void = () => undefined
    const collectStarted = new Promise<void>((resolve) => {
      markCollectStarted = resolve
    })
    mocks.collectNetworkInfo.mockImplementationOnce(async () => await new Promise<NetworkInfoResponse>((resolve) => {
      resolveNetworkInfo = resolve
      markCollectStarted()
    }))

    const staleUpdate = vpn.onNetworkInstanceUpdate('A')
    await collectStarted
    const newerUpdate = vpn.onNetworkInstanceChange('B')
    resolveNetworkInfo({
      info: {
        map: {
          A: {
            my_node_info: {
              virtual_ipv4: {
                address: { addr: '10.0.0.99' },
                network_length: 24,
              },
            },
            routes: [],
          },
        },
      },
    })

    await Promise.all([staleUpdate, newerUpdate])

    expect(mocks.startVpn).not.toHaveBeenCalled()
    expect(mocks.stopVpn).not.toHaveBeenCalled()
  })

  it('preserves a native VPN while network info is unavailable', async () => {
    setConfig('A')
    mocks.getVpnStatus.mockResolvedValue({
      running: true,
      ipv4Addr: '10.0.0.1/24',
      routes: [],
    })
    mocks.listNetworkInstanceIds.mockResolvedValue({ running_inst_ids: ['A'] })
    const vpn = await loadVpnModule()

    await vpn.syncMobileVpnService()

    expect(mocks.stopVpn).not.toHaveBeenCalled()
    expect(mocks.startVpn).not.toHaveBeenCalled()
  })
})

describe('mobile VPN generation and capture policy', () => {
  it('uses Core capture routes with physical DNS literals', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    mocks.getMobileTunConfig.mockResolvedValue({
      ...makeMobileTunConfig(['A']),
      addresses: ['10.0.0.1/24', 'fd42::1/64'],
      routes: ['0.0.0.0/0', '::/0'],
    })
    mocks.setTunFd.mockImplementation(async () => mocks.getMobileTunConfig(['A']))
    mocks.getUnderlyingDnsServers.mockResolvedValue({ dnsServers: ['fe80::1%wlan0'], systemDnsServers: ['fe80::1'] })
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    expect(mocks.startVpn).toHaveBeenCalledWith(expect.objectContaining({
      ipv4Addrs: ['10.0.0.1/24', 'fd42::1/64'],
      routes: ['0.0.0.0/0', '::/0'],
      dnsServers: ['fe80::1%wlan0'],
    }))
  })

  it('advances beyond the native generation high water after frontend reload', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    mocks.getVpnStatus.mockResolvedValue({ running: false, generationHighWater: '9007199254740993' })
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    expect(mocks.startVpn).toHaveBeenCalledWith(expect.objectContaining({ generation: '9007199254740994' }))
  })

  it('reconciles a stale native attachment acknowledgement with a new generation', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    mocks.completeVpnReconfigure.mockResolvedValueOnce({ accepted: false })
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    await vi.advanceTimersByTimeAsync(2000)
    expect(mocks.startVpn).toHaveBeenCalledTimes(2)
    expect(mocks.completeVpnReconfigure).toHaveBeenLastCalledWith(expect.any(String), true)
  })

  it('keeps a newer attachment when an older stop finishes detaching', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    const oldGeneration = mocks.startVpn.mock.calls[0][0].generation
    let finishDetach!: (config: MobileTunConfig) => void
    mocks.setTunFd.mockImplementationOnce(() => new Promise(resolve => { finishDetach = resolve }))

    const oldStop = mocks.listeners.get('vpn_service_stop')!({ generation: oldGeneration })
    expect(mocks.setTunFd).toHaveBeenLastCalledWith(0, undefined, oldGeneration)
    mocks.getCoreVpnStatus.mockResolvedValue({ policy_generation: '2' })
    await vpn.onNetworkInstanceUpdate('A')
    const newGeneration = mocks.startVpn.mock.calls[1][0].generation
    expect(newGeneration).not.toBe(oldGeneration)
    expect(mocks.completeVpnReconfigure).toHaveBeenLastCalledWith(newGeneration, true)

    finishDetach({ instance_ids: [], addresses: [], routes: [], rejected: [] })
    await oldStop
    await vpn.onNetworkInstancesChange()
    expect(mocks.startVpn).toHaveBeenCalledTimes(2)
    expect(mocks.stopVpn).not.toHaveBeenCalled()
  })

  it('keeps a newer attachment when an older acknowledgement is rejected', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    const oldGeneration = mocks.startVpn.mock.calls[0][0].generation
    let finishAcknowledgement!: (result: { accepted: boolean }) => void
    mocks.completeVpnReconfigure.mockImplementationOnce(() => new Promise((resolve) => {
      finishAcknowledgement = resolve
    }))

    const oldAttachment = mocks.listeners.get('vpn_service_start')!({
      fd: 1, generation: oldGeneration, instanceIds: ['A'],
    })
    await vi.waitFor(() => expect(finishAcknowledgement).toBeDefined())
    mocks.getCoreVpnStatus.mockResolvedValue({ policy_generation: '2' })
    await vpn.onNetworkInstanceUpdate('A')
    const newGeneration = mocks.startVpn.mock.calls[1][0].generation
    expect(mocks.completeVpnReconfigure).toHaveBeenLastCalledWith(newGeneration, true)

    finishAcknowledgement({ accepted: false })
    await oldAttachment
    await vpn.onNetworkInstancesChange()
    expect(mocks.startVpn).toHaveBeenCalledTimes(2)
    expect(mocks.stopVpn).not.toHaveBeenCalled()
  })

  it('keeps service cleanup available after a newer start fails during an older stop', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    const oldGeneration = mocks.startVpn.mock.calls[0][0].generation
    await mocks.listeners.get('vpn_service_error')!({ generation: oldGeneration, error: 'old candidate failed' })
    let finishDetach!: (config: MobileTunConfig) => void
    mocks.setTunFd.mockImplementationOnce(() => new Promise(resolve => { finishDetach = resolve }))
    const oldStop = mocks.listeners.get('vpn_service_stop')!({ generation: oldGeneration })

    mocks.startVpn.mockImplementationOnce(async () => ({}))
    const newStart = vpn.onNetworkInstanceUpdate('A')
    await vi.advanceTimersByTimeAsync(50)
    const newGeneration = mocks.startVpn.mock.calls[1][0].generation
    await mocks.listeners.get('vpn_service_error')!({ generation: newGeneration, error: 'new candidate failed' })
    await vi.advanceTimersByTimeAsync(50)
    await newStart

    finishDetach({ instance_ids: [], addresses: [], routes: [], rejected: [] })
    await oldStop
    mocks.listNetworkInstanceIds.mockResolvedValue({ running_inst_ids: [] })
    await vpn.onNetworkInstancesChange()
    expect(mocks.stopVpn).toHaveBeenCalledOnce()
  })

  it('retries interface establishment errors without stopping capture', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    const generation = mocks.startVpn.mock.calls[0][0].generation
    await mocks.listeners.get('vpn_service_error')?.({ generation, error: 'establish failed' })
    await vi.advanceTimersByTimeAsync(2000)
    expect(mocks.startVpn).toHaveBeenCalledTimes(2)
    expect(mocks.stopVpn).not.toHaveBeenCalled()
  })

  it('replaces an unchanged interface when Core policy generation changes', async () => {
    setConfig('A')
    setReady('A', '10.0.0.1')
    const vpn = await loadVpnModule()
    await vpn.onNetworkInstanceChange('A')
    const oldGeneration = mocks.startVpn.mock.calls[0][0].generation
    mocks.getCoreVpnStatus.mockResolvedValue({ policy_generation: '2' })
    await vpn.onNetworkInstanceUpdate('A')
    expect(mocks.startVpn).toHaveBeenCalledTimes(2)
    expect(mocks.startVpn.mock.calls[1][0].generation).not.toBe(oldGeneration)
    expect(mocks.stopVpn).not.toHaveBeenCalled()
    mocks.setTunFd.mockClear()
    await mocks.listeners.get('vpn_service_start')?.({ fd: 7, generation: oldGeneration, instanceIds: ['A'] })
    await mocks.listeners.get('vpn_service_stop')?.({ generation: oldGeneration })
    expect(mocks.setTunFd).not.toHaveBeenCalled()
    await vpn.onNetworkInstancesChange()
    expect(mocks.startVpn).toHaveBeenCalledTimes(2)
  })
})

describe('mobile VPN tile action delivery', () => {
  it('does not consume a pending action before a handler is ready', async () => {
    const vpn = await loadVpnModule()

    expect(await vpn.consumePendingMobileVpnTileAction()).toBe(false)
    expect(mocks.consumeVpnTileAction).not.toHaveBeenCalled()
  })

  it('consumes and dispatches a pending action once a handler is registered', async () => {
    const vpn = await loadVpnModule()
    const handler = vi.fn(async () => undefined)
    mocks.consumeVpnTileAction.mockResolvedValue({ action: 'start' })
    vpn.setMobileVpnTileActionHandler(handler)

    expect(await vpn.consumePendingMobileVpnTileAction()).toBe(true)
    expect(handler).toHaveBeenCalledWith('start')
  })
})
