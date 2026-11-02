import { useRoute, useRouter } from 'vue-router'

/**
 * Device management opens over the current page: the device lives in the
 * query, so closing it returns to the same page at the same place.
 */
export function useDevicePopup() {
  const route = useRoute()
  const router = useRouter()
  return {
    open(deviceId: string, instanceId?: string) {
      void router.push({ query: { ...route.query, device: deviceId, instance: instanceId || undefined } })
    },
  }
}
