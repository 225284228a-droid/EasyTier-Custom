import { useRoute, useRouter } from 'vue-router'

/**
 * Device management opens over the current page: the device lives in the
 * query, so closing it returns to the same page at the same place.
 */
export function useDevicePopup() {
  const route = useRoute()
  const router = useRouter()
  return {
    /** `edit` opens the network's form instead of its status. */
    open(deviceId: string, instanceId?: string, edit = false) {
      void router.push({ query: { ...route.query, device: deviceId, instance: instanceId || undefined, edit: edit ? '1' : undefined } })
    },
  }
}
