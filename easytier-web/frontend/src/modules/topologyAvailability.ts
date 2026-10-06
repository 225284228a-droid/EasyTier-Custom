import type { Utils } from 'easytier-frontend-lib'

export const TOPOLOGY_WARNING_DELAY_MS = 60_000

/** Measures missing reports independently of retries, rendering and cache expiry. */
export class TopologyAvailability {
  private reports = new Map<string, number>()
  private listReportedAt: number
  private listFailed = false

  constructor(time: number) {
    this.listReportedAt = time
  }

  clear(time: number) {
    this.reports.clear()
    this.listReportedAt = time
    this.listFailed = false
  }

  updateDevices(devices: Utils.DeviceInfo[], time: number) {
    this.listReportedAt = time
    this.listFailed = false
    const running = new Set(devices.filter(device => device.running_network_count > 0)
      .map(device => device.machine_id))
    for (const id of this.reports.keys()) {
      if (!running.has(id))
        this.reports.delete(id)
    }
    for (const id of running) {
      if (!this.reports.has(id))
        this.reports.set(id, time)
    }
  }

  report(machineId: string, time: number) {
    if (this.reports.has(machineId))
      this.reports.set(machineId, time)
  }

  failListing() {
    this.listFailed = true
  }

  listUnavailable(time: number) {
    return this.listFailed && time - this.listReportedAt >= TOPOLOGY_WARNING_DELAY_MS
  }

  incomplete(time: number) {
    return [...this.reports.values()].some(reportedAt =>
      time - reportedAt >= TOPOLOGY_WARNING_DELAY_MS)
  }

  nextDeadline(time: number): number | undefined {
    const times = [...this.reports.values()]
    if (this.listFailed)
      times.push(this.listReportedAt)
    let next: number | undefined
    for (const reportedAt of times) {
      const deadline = reportedAt + TOPOLOGY_WARNING_DELAY_MS
      if (deadline > time)
        next = next === undefined ? deadline : Math.min(next, deadline)
    }
    return next
  }
}
