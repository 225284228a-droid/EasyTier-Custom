import type { MobileTunConfig } from './backend'
import { addPluginListener } from '@tauri-apps/api/core'
import { Utils } from 'easytier-frontend-lib'
import {
  complete_vpn_reconfigure,
  consume_vpn_tile_action,
  get_underlying_dns_servers,
  reconfigure_vpn,
  get_vpn_status,
  prepare_vpn,
  start_vpn,
  stop_vpn,
  type VpnTileAction,
} from 'tauri-plugin-vpnservice-api'
import { collectNetworkInfo, getConfig, getMobileTunConfig, getVpnStatus, listNetworkInstanceIds, setTunFd } from './backend'

interface vpnStatus {
  running: boolean
  ipv4Addrs: string[]
  routes: string[]
  dnsServers: string[]
  generation: string | undefined
  generationHighWater: string | undefined
  policyGeneration: string | undefined
}

let vpnReconcileTimer: ReturnType<typeof setTimeout> | null = null
const VPN_RECONCILE_INTERVAL_MS = 2000
const VPN_RECONCILE_MAX_ATTEMPTS = 60

const vpnTunRetryTimers = new Map<string, ReturnType<typeof setTimeout>>()
const VPN_GENERATION_STORAGE_KEY = 'easytier.vpn.configuration-generation'
const MAX_VPN_GENERATION = (1n << 63n) - 1n
let vpnConfigurationGeneration = 0n

function parseVpnGeneration(value: unknown) {
  if (typeof value !== 'string' || !/^\d+$/.test(value)) return 0n
  try {
    const parsed = BigInt(value)
    return parsed <= MAX_VPN_GENERATION ? parsed : 0n
  }
  catch {
    return 0n
  }
}

async function nextVpnConfigurationGeneration() {
  let highWater = vpnConfigurationGeneration
  try {
    const stored = parseVpnGeneration(localStorage.getItem(VPN_GENERATION_STORAGE_KEY))
    if (stored > highWater) highWater = stored
  }
  catch {
    // The in-memory counter remains available in restricted WebView contexts.
  }

  try {
    const status = await get_vpn_status()
    const nativeHighWater = parseVpnGeneration(status?.generationHighWater)
    if (nativeHighWater > highWater) highWater = nativeHighWater
    curVpnStatus.generationHighWater = status?.generationHighWater ?? curVpnStatus.generationHighWater
  }
  catch (error) {
    console.warn('vpn generation high-water query failed', error)
  }

  const next = highWater + 1n
  if (next > MAX_VPN_GENERATION) throw new Error('vpn generation counter exhausted')
  vpnConfigurationGeneration = next
  try {
    localStorage.setItem(VPN_GENERATION_STORAGE_KEY, next.toString())
  }
  catch {
    // The Android high-water mark is also persisted by the native service.
  }
  return next.toString()
}

let vpnReconcileGeneration = 0
let vpnReconcileAttempts = 0
let vpnReconcileQueue: Promise<void> = Promise.resolve()
let vpnPermissionRequest: Promise<boolean> | null = null
let vpnTileActionHandler: ((action: VpnTileAction) => Promise<void>) | undefined
let vpnTileActionQueue: Promise<void> = Promise.resolve()
let vpnAttachedInstanceIds: string[] = []
let vpnStartingInstanceIds: string[] | undefined
let vpnAttachment: Promise<MobileTunConfig | undefined> | null = null
let vpnErrorHandler: ((error: string) => void) | undefined
let vpnMemberErrors = ''

export function setMobileVpnErrorHandler(handler?: (error: string) => void) {
  vpnErrorHandler = handler
}

function reportRejectedMembers(config: MobileTunConfig) {
  const errors = config.rejected.map(({ instance_id, error }) => `${instance_id}: ${error}`).sort().join('\n')
  if (errors && errors !== vpnMemberErrors)
    vpnErrorHandler?.(errors)
  vpnMemberErrors = errors
}

const curVpnStatus: vpnStatus = {
  running: false,
  ipv4Addrs: [],
  routes: [],
  dnsServers: [],
  generation: undefined,
  generationHighWater: undefined,
  policyGeneration: undefined,
}

export function setMobileVpnTileActionHandler(
  handler?: (action: VpnTileAction) => Promise<void>,
) {
  vpnTileActionHandler = handler
}

export async function consumePendingMobileVpnTileAction() {
  const handler = vpnTileActionHandler
  if (!handler) {
    return false
  }

  const action = (await consume_vpn_tile_action())?.action
  if (action !== 'start' && action !== 'stop') {
    return false
  }

  const run = vpnTileActionQueue
    .catch(error => console.error('previous VPN tile action failed', error))
    .then(() => handler(action))
  vpnTileActionQueue = run.catch(error => console.error('VPN tile action failed', error))
  await run
  return true
}

async function requestVpnPermissionOnce() {
  console.log('prepare vpn')
  const prepare_ret = await prepare_vpn()
  console.log('prepare vpn', JSON.stringify((prepare_ret)))
  if (prepare_ret?.errorMsg?.length) {
    throw new Error(prepare_ret.errorMsg)
  }

  const granted = prepare_ret?.granted ?? true
  if (!granted) {
    console.info('vpn permission request was denied or dismissed')
  }

  return granted
}

async function requestVpnPermission() {
  if (vpnPermissionRequest) {
    console.log('reuse pending vpn permission request')
    return await vpnPermissionRequest
  }

  const request = requestVpnPermissionOnce()
  vpnPermissionRequest = request
  try {
    return await request
  }
  finally {
    if (vpnPermissionRequest === request) {
      vpnPermissionRequest = null
    }
  }
}

function clearVpnReconcileTimer() {
  if (vpnReconcileTimer) {
    clearTimeout(vpnReconcileTimer)
    vpnReconcileTimer = null
  }
}

function beginVpnReconcile() {
  clearVpnReconcileTimer()
  vpnReconcileAttempts = 0
  vpnReconcileGeneration += 1
  return vpnReconcileGeneration
}

function isCurrentVpnReconcile(generation: number) {
  return vpnReconcileGeneration === generation
}

function scheduleVpnReconcile(generation: number, reason: string) {
  if (!isCurrentVpnReconcile(generation))
    return

  if (vpnReconcileAttempts >= VPN_RECONCILE_MAX_ATTEMPTS) {
    console.error(
      'vpn service reconcile stopped after maximum attempts',
      VPN_RECONCILE_MAX_ATTEMPTS,
      reason,
    )
    return
  }

  clearVpnReconcileTimer()
  vpnReconcileAttempts += 1
  console.log(
    'vpn service is not ready, retrying',
    JSON.stringify({
      attempt: vpnReconcileAttempts,
      maxAttempts: VPN_RECONCILE_MAX_ATTEMPTS,
      delayMs: VPN_RECONCILE_INTERVAL_MS,
      reason,
    }),
  )
  vpnReconcileTimer = setTimeout(() => {
    vpnReconcileTimer = null
    void enqueueVpnReconcile(generation)
  }, VPN_RECONCILE_INTERVAL_MS)
}

function resetVpnConfigStatus() {
  vpnAttachedInstanceIds = []
  curVpnStatus.ipv4Addrs = []
  curVpnStatus.routes = []
  curVpnStatus.dnsServers = []
  curVpnStatus.generation = undefined
  curVpnStatus.policyGeneration = undefined
}

function syncVpnStatusFromNative(status: Awaited<ReturnType<typeof get_vpn_status>>) {
  curVpnStatus.running = status?.running ?? false
  curVpnStatus.generationHighWater = status?.generationHighWater ?? undefined
  if (!curVpnStatus.running) {
    resetVpnConfigStatus()
    return
  }

  curVpnStatus.ipv4Addrs = [...(status?.ipv4Addrs ?? [])]
  curVpnStatus.routes = [...(status?.routes ?? [])]
  curVpnStatus.dnsServers = [...(status?.dnsServers ?? [])]
  curVpnStatus.generation = status?.generation ?? undefined
}

async function waitVpnStatus(target_status: boolean, timeout_sec: number) {
  const start_time = Date.now()
  while (curVpnStatus.running !== target_status) {
    if (Date.now() - start_time > timeout_sec * 1000) {
      throw new Error('wait vpn status timeout')
    }
    await new Promise(r => setTimeout(r, 50))
  }
}

async function detachTunFd(generation?: string) {
  try {
    if (generation)
      await setTunFd(0, undefined, generation)
    else
      await setTunFd(0)
  }
  catch (e) {
    console.error('detach tun fd failed', e)
  }
}

async function doStopVpn(force = false) {
  const wasRunning = curVpnStatus.running
  if (!force && !wasRunning) {
    return
  }
  await detachTunFd()
  console.log('stop vpn')
  const stop_ret = await stop_vpn()
  console.log('stop vpn', JSON.stringify((stop_ret)))
  if (wasRunning) {
    await waitVpnStatus(false, 3)
  }

  resetVpnConfigStatus()
}

async function doStartVpn(
  instanceIds: string[],
  ipv4Addrs: string[],
  routes: string[],
  dnsServers: string[],
  policyGeneration: string,
) {
  const generation = await nextVpnConfigurationGeneration()
  vpnStartingInstanceIds = instanceIds
  vpnAttachment = null
  const request = {
    ipv4Addrs,
    routes,
    dnsServers,
    disallowedApplications: [],
    mtu: 1300,
    instanceIds,
    generation,
  }
  // Publish the candidate generation before native events can arrive.
  curVpnStatus.generation = generation
  curVpnStatus.generationHighWater = generation
  let configure = curVpnStatus.running ? reconfigure_vpn : start_vpn
  let result = await configure(request)
  if (result?.errorMsg === 'need_prepare') {
    if (!await requestVpnPermission()) throw new Error('vpn_permission_denied')
    configure = curVpnStatus.running ? reconfigure_vpn : start_vpn
    result = await configure(request)
  }
  if (result?.errorMsg?.length) throw new Error(result.errorMsg)
  const actual = await waitVpnAttachment()
  curVpnStatus.ipv4Addrs = [...ipv4Addrs]
  curVpnStatus.routes = [...routes]
  curVpnStatus.dnsServers = [...dnsServers]
  curVpnStatus.policyGeneration = policyGeneration
  if (actual) {
    vpnAttachedInstanceIds = [...actual.instance_ids].sort()
    if (JSON.stringify([...actual.addresses].sort()) !== JSON.stringify(ipv4Addrs)
      || JSON.stringify([...actual.routes].sort()) !== JSON.stringify(routes))
      scheduleVpnReconcile(vpnReconcileGeneration, 'attachment_configuration_changed')
  }
}

async function waitVpnAttachment() {
  const generation = curVpnStatus.generation
  const started = Date.now()
  while (!vpnAttachment && curVpnStatus.generation === generation) {
    if (Date.now() - started > 3000) throw new Error('wait vpn attachment timeout')
    await new Promise(resolve => setTimeout(resolve, 50))
  }
  // Failed installation retains the native capture candidate and retries it.
  return await vpnAttachment
}

function clearVpnTunRetry(generation: string) {
  const timer = vpnTunRetryTimers.get(generation)
  if (timer) clearTimeout(timer)
  vpnTunRetryTimers.delete(generation)
}

function scheduleVpnTunRetry(payload: any, attempt: number) {
  const generation = String(payload?.generation ?? '')
  if (!generation || generation !== curVpnStatus.generation || !curVpnStatus.running)
    return

  if (attempt >= VPN_RECONCILE_MAX_ATTEMPTS) {
    console.error('vpn TUN attach retry stopped after maximum attempts', generation)
    return
  }

  clearVpnTunRetry(generation)
  const timer = setTimeout(() => {
    vpnTunRetryTimers.delete(generation)
    void attachVpnTun(payload, attempt + 1)
  }, VPN_RECONCILE_INTERVAL_MS)
  vpnTunRetryTimers.set(generation, timer)
}

async function attachVpnTun(payload: any, attempt = 0) {
  const generation = String(payload?.generation ?? '')
  const instanceIds: string[] = payload?.instanceIds ?? vpnStartingInstanceIds ?? []
  if (!generation || !instanceIds.length || generation !== curVpnStatus.generation || !payload?.fd)
    return

  try {
    const actual = await setTunFd(payload.fd, instanceIds, generation)
    if (generation !== curVpnStatus.generation) return
    reportRejectedMembers(actual)
    vpnAttachedInstanceIds = [...actual.instance_ids].sort()
    if (!actual.instance_ids.length) {
      await doStopVpn(true)
      return
    }
    const completion = await complete_vpn_reconfigure(generation, true)
    if (generation !== curVpnStatus.generation) return
    clearVpnTunRetry(generation)
    if (!completion?.accepted) {
      console.info('vpn TUN attach acknowledgement was stale', generation)
      curVpnStatus.generation = undefined
      scheduleVpnReconcile(vpnReconcileGeneration, 'vpn_attachment_acknowledgement_stale')
      return
    }
    if (attempt > 0 && (JSON.stringify([...actual.addresses].sort()) !== JSON.stringify(curVpnStatus.ipv4Addrs)
      || JSON.stringify([...actual.routes].sort()) !== JSON.stringify(curVpnStatus.routes)))
      scheduleVpnReconcile(vpnReconcileGeneration, 'attachment_configuration_changed')
    return actual
  }
  catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    console.error('set tun fd failed; retaining Android capture interface', generation, message)
    await complete_vpn_reconfigure(generation, false, message).catch((completionError) => {
      console.error('vpn install failure report failed', completionError)
    })
    scheduleVpnTunRetry(payload, attempt)
  }
}

async function onVpnServiceStart(payload: any) {
  const generation = String(payload?.generation ?? '')
  if (!generation || (curVpnStatus.generation && generation !== curVpnStatus.generation)) return
  curVpnStatus.running = true
  curVpnStatus.generation = generation
  vpnAttachment = attachVpnTun(payload)
  await vpnAttachment
}

async function onVpnServiceError(payload: any) {
  if (payload?.generation !== curVpnStatus.generation) return
  console.error('vpn interface configuration failed', payload?.error)
  curVpnStatus.generation = undefined
  curVpnStatus.policyGeneration = undefined
  scheduleVpnReconcile(vpnReconcileGeneration, 'vpn_interface_establish_failed')
}

async function onVpnServiceStop(payload: any) {
  if (payload?.generation && curVpnStatus.generation && payload.generation !== curVpnStatus.generation) return
  const generation = curVpnStatus.generation
  const configurationGeneration = vpnConfigurationGeneration
  console.log('vpn service stop', JSON.stringify(payload))
  for (const generation of vpnTunRetryTimers.keys()) clearVpnTunRetry(generation)
  await detachTunFd(payload?.generation || generation)
  // A newer start can fail and clear its status generation again while this
  // detach is pending. The monotonic counter also protects that empty state.
  if (generation !== curVpnStatus.generation || configurationGeneration !== vpnConfigurationGeneration) return
  curVpnStatus.running = false
  resetVpnConfigStatus()
}

async function registerVpnServiceListener() {
  console.log('register vpn service listener')
  await addPluginListener(
    'vpnservice',
    'vpn_service_start',
    onVpnServiceStart,
  )

  await addPluginListener(
    'vpnservice',
    'vpn_service_stop',
    onVpnServiceStop,
  )

  await addPluginListener('vpnservice', 'vpn_service_error', onVpnServiceError)

  await addPluginListener(
    'vpnservice',
    'vpn_tile_action',
    () => {
      void consumePendingMobileVpnTileAction().catch((error) => {
        console.error('consume VPN tile action failed', error)
      })
    },
  )
}

async function reconcileNetworkInstance(generation: number) {
  if (!isCurrentVpnReconcile(generation))
    return

  clearVpnReconcileTimer()

  let instances: Awaited<ReturnType<typeof findRunningTunInstances>>
  try {
    instances = await findRunningTunInstances()
  }
  catch (error) {
    console.warn('vpn service instance query failed', error)
    scheduleVpnReconcile(generation, 'instance_list_unavailable')
    return
  }

  if (!isCurrentVpnReconcile(generation))
    return

  if (!instances.length) {
    if (curVpnStatus.running)
      await doStopVpn()
    return
  }

  let retryReason: string | undefined
  let networkInfoUnavailable = false

  for (const { instanceId: memberId, config } of instances) {
    let curNetworkInfo
    try {
      curNetworkInfo = (await collectNetworkInfo(memberId)).info.map[memberId]
    }
    catch (error) {
      console.warn('vpn service network info query failed', memberId, error)
      retryReason ??= 'network_info_query_failed'
      networkInfoUnavailable = true
      continue
    }

    if (!isCurrentVpnReconcile(generation))
      return

    if (!curNetworkInfo) {
      console.warn('vpn service network info unavailable', memberId)
      retryReason ??= 'network_info_unavailable'
      networkInfoUnavailable = true
      continue
    }

    if (curNetworkInfo.error_msg?.length) {
      console.warn('vpn service network failed', memberId, curNetworkInfo.error_msg)
      retryReason ??= 'network_failed'
      continue
    }

    const virtualIpv4 = curNetworkInfo.my_node_info?.virtual_ipv4
    const virtualIpv6 = curNetworkInfo.my_node_info?.virtual_ipv6
    const dhcp = config.dhcp || config.dhcpv6
    if (!virtualIpv4?.address?.addr && !virtualIpv6?.address) {
      if (!dhcp && !config.virtual_ipv4 && curNetworkInfo.my_node_info)
        continue
      retryReason ??= dhcp ? 'dhcp_address_unavailable' : 'static_address_unavailable'
      if (!dhcp)
        networkInfoUnavailable = true
      continue
    }

  }

  if (!isCurrentVpnReconcile(generation))
    return

  if (networkInfoUnavailable && curVpnStatus.running) {
    scheduleVpnReconcile(generation, retryReason || 'network_info_unavailable')
    return
  }

  const preferredIds = [...instances].sort((a, b) =>
    Number(vpnAttachedInstanceIds.includes(b.instanceId)) - Number(vpnAttachedInstanceIds.includes(a.instanceId))
    || a.instanceId.localeCompare(b.instanceId)).map(({ instanceId }) => instanceId)
  let config: MobileTunConfig
  try {
    config = await getMobileTunConfig(preferredIds)
  }
  catch (error) {
    console.error('mobile TUN configuration query failed', error)
    scheduleVpnReconcile(generation, 'tun_configuration_unavailable')
    return
  }
  if (!isCurrentVpnReconcile(generation))
    return
  reportRejectedMembers(config)
  if (!config.addresses.length) {
    if (retryReason)
      scheduleVpnReconcile(generation, retryReason)
    if (curVpnStatus.running)
      await doStopVpn()
    return
  }

  if (retryReason)
    scheduleVpnReconcile(generation, retryReason)
  else
    vpnReconcileAttempts = 0
  const sortedIpv4Addrs = [...config.addresses].sort()
  const sortedRoutes = [...config.routes].sort()
  const instanceIds = [...config.instance_ids].sort()
  let dnsServers: string[]
  let policyGeneration: string
  try {
    const dns = await get_underlying_dns_servers()
    if (dns.errorMsg) throw new Error(dns.errorMsg)
    dnsServers = config.dns ? [config.dns] : [...new Set(dns.dnsServers)].sort()
    const policies = await Promise.all(instanceIds.map(async (id) => {
      const policy = await getVpnStatus(id)
      if (!policy) throw new Error(`VPN policy unavailable: ${id}`)
      return `${id}:${policy.policy_generation}`
    }))
    policyGeneration = policies.join(',')
  }
  catch (error) {
    console.warn('vpn core policy or DNS unavailable', error)
    scheduleVpnReconcile(generation, 'core_state_query_failed')
    return
  }
  if (!isCurrentVpnReconcile(generation)) return
  // A replacement runtime needs its own attachment even with identical OS routes.
  const membersChanged = JSON.stringify(instanceIds) !== JSON.stringify(vpnAttachedInstanceIds)
  const configChanged
    = JSON.stringify(sortedIpv4Addrs) !== JSON.stringify(curVpnStatus.ipv4Addrs)
      || JSON.stringify(sortedRoutes) !== JSON.stringify(curVpnStatus.routes)
      || JSON.stringify(dnsServers) !== JSON.stringify(curVpnStatus.dnsServers)
      || policyGeneration !== curVpnStatus.policyGeneration
      || !curVpnStatus.generation

  if (!curVpnStatus.running || configChanged || membersChanged) {
    if (!isCurrentVpnReconcile(generation))
      return

    try {
      await doStartVpn(instanceIds, sortedIpv4Addrs, sortedRoutes, dnsServers, policyGeneration)
    }
    catch (error) {
      if (error instanceof Error && error.message === 'vpn_permission_denied') {
        console.info('vpn permission request was denied or dismissed')
        return
      }
      console.error('start vpn service failed', error)
      scheduleVpnReconcile(generation, 'vpn_configuration_failed')
    }
  }
}

function enqueueVpnTask(task: () => Promise<void>) {
  const run = vpnReconcileQueue
    .catch((e) => {
      console.error('previous vpn service reconcile failed', e)
    })
    .then(task)
  vpnReconcileQueue = run.catch((e) => {
    console.error('vpn service reconcile failed', e)
  })
  return run
}

function enqueueVpnReconcile(generation: number) {
  return enqueueVpnTask(() => reconcileNetworkInstance(generation))
}

export async function onNetworkInstanceChange(instanceId: string) {
  // A restarted instance keeps its ID but loses the previous runtime's TUN attachment.
  vpnAttachedInstanceIds = vpnAttachedInstanceIds.filter(id => id !== instanceId)
  await onNetworkInstancesChange()
}

export async function onNetworkInstancesChange() {
  await enqueueVpnReconcile(beginVpnReconcile())
}

export async function onNetworkInstanceUpdate(instanceId: string) {
  if (!instanceId)
    return
  await onNetworkInstancesChange()
}

async function isNoTunEnabled(instanceId: string | undefined) {
  if (!instanceId) {
    return false
  }
  return (await getConfig(instanceId)).no_tun ?? false
}

async function findRunningTunInstances() {
  const instanceIds = await listNetworkInstanceIds()
  const runningIds = (instanceIds.running_inst_ids ?? []).map(Utils.UuidToStr)
  const runningTunInstances = []

  for (const instanceId of runningIds) {
    const config = await getConfig(instanceId)
    if (config.no_tun)
      continue
    runningTunInstances.push({ instanceId, config })
  }

  return runningTunInstances
}

export async function initMobileVpnService() {
  await registerVpnServiceListener()
}

export async function prepareVpnService(instanceId: string) {
  if (await isNoTunEnabled(instanceId)) {
    return
  }

  await requestVpnPermission()
}

export async function syncMobileVpnService() {
  syncVpnStatusFromNative(await get_vpn_status())
  await onNetworkInstanceChange('')
}
