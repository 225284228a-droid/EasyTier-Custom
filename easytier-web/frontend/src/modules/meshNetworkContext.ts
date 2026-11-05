import { inject, provide, type ComputedRef, type InjectionKey, type Ref } from 'vue'
import type ApiClient from './api'
import type { MeshNetworkView, MeshSyncResult, MeshJoinResult } from './api'
import type { DiscoveredNetwork, NetworkMember } from './networkDiscovery'

/** What the pages of one network share: the network and the console's record of it. */
export interface MeshNetworkContext {
  api: ApiClient
  networkName: ComputedRef<string>
  network: ComputedRef<DiscoveredNetwork | undefined>
  /** The console's view: core settings, preset, joining on their own. Central mode only. */
  view: Ref<MeshNetworkView | undefined>
  centralEnabled: ComputedRef<boolean>
  reload(): Promise<void>
}

/** A network's pages; the central ones need the console to manage devices itself. */
export const NETWORK_SECTIONS = [
  { name: 'meshNetwork', label: 'web.mesh_network.section_overview', icon: 'pi pi-chart-bar', central: false },
  { name: 'meshMembers', label: 'web.mesh_network.section_members', icon: 'pi pi-server', central: false },
  { name: 'meshSettings', label: 'web.mesh_network.section_settings', icon: 'pi pi-shield', central: true },
  { name: 'meshAdvanced', label: 'web.mesh_network.section_advanced', icon: 'pi pi-sliders-h', central: true },
  { name: 'meshAcl', label: 'web.mesh_network.section_acl', icon: 'pi pi-lock', central: true },
  { name: 'meshPreset', label: 'web.mesh_network.section_preset', icon: 'pi pi-box', central: true },
  { name: 'meshTemporary', label: 'web.mesh_network.section_temporary', icon: 'pi pi-clock', central: true },
] as const

const KEY: InjectionKey<MeshNetworkContext> = Symbol('meshNetwork')

export function provideMeshNetwork(context: MeshNetworkContext) {
  provide(KEY, context)
}

export function useMeshNetwork(): MeshNetworkContext {
  const context = inject(KEY, undefined)
  if (!context) throw new Error('Mesh network context is not provided')
  return context
}

export const deviceName = (member: Pick<NetworkMember, 'device'>) =>
  member.device.alias || member.device.hostname || member.device.machine_id

export type SyncResult = MeshSyncResult | MeshJoinResult

/** Results that did not reach their member as asked. */
export const failedResults = (results: SyncResult[]) =>
  results.filter(result => !['saved', 'unchanged', 'pending', 'locked'].includes(result.status))

export const resultSeverity = (status: SyncResult['status']) =>
  status === 'saved' || status === 'unchanged' ? 'success'
    : status === 'pending' || status === 'locked' ? 'info'
      : status === 'failed' ? 'danger' : 'warn'
