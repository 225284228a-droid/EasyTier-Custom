export interface FlowLabelInput {
  sourceLabel: string
  targetLabel: string
  txBps?: number
  rxBps?: number
  latencyMs?: number
}

function formatRate(value: number | undefined): string {
  if (value === undefined || !Number.isFinite(value))
    return '--'
  const units = ['bit/s', 'kbit/s', 'Mbit/s', 'Gbit/s', 'Tbit/s']
  let unit = 0
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1000
    unit++
  }
  return `${value.toFixed(value < 10 && unit ? 2 : value < 100 && unit ? 1 : 0)} ${units[unit]}`
}

function formatLatency(value: number | undefined): string {
  if (value === undefined || !Number.isFinite(value) || value <= 0)
    return '--'
  return `${value.toFixed(value < 10 ? 2 : value < 100 ? 1 : 0)} ms`
}

export function updateFlowLabel(cross: HTMLDivElement, input: FlowLabelInput): void {
  const forward = `${input.sourceLabel} \u2192 ${input.targetLabel}`
  const reverse = `${input.targetLabel} \u2192 ${input.sourceLabel}`
  const sourceStats = formatRate(input.txBps)
  const targetStats = formatRate(input.rxBps)
  const latency = `RTT ${formatLatency(input.latencyMs)}`
  const description = `${forward}: ${sourceStats}; ${reverse}: ${targetStats}; ${latency}`
  if (cross.getAttribute('aria-label') !== description)
    cross.setAttribute('aria-label', description)

  for (const [selector, text, title] of [
    ['.globe-flow-source-stat', sourceStats, `${forward}: ${sourceStats}`],
    ['.globe-flow-target-stat', targetStats, `${reverse}: ${targetStats}`],
    ['.globe-flow-source', input.sourceLabel, input.sourceLabel],
    ['.globe-flow-target', input.targetLabel, input.targetLabel],
    ['.globe-flow-latency-value', latency, 'Round-trip latency (RTT)'],
  ]) {
    const element = cross.querySelector<HTMLElement>(selector)!
    if (element.textContent !== text)
      element.textContent = text
    if (element.title !== title)
      element.title = title
  }
}

export function createFlowLabel(input: FlowLabelInput): HTMLDivElement {
  const cross = document.createElement('div')
  cross.className = 'globe-flow-cross'
  const top = document.createElement('div')
  top.className = 'globe-flow-stat globe-flow-source-stat'
  const bottom = document.createElement('div')
  bottom.className = 'globe-flow-stat globe-flow-target-stat'
  const source = document.createElement('span')
  source.className = 'globe-flow-name globe-flow-source'
  const target = document.createElement('span')
  target.className = 'globe-flow-name globe-flow-target'
  const directions = document.createElement('i')
  directions.className = 'pi pi-arrow-right-arrow-left globe-flow-directions'
  directions.setAttribute('aria-hidden', 'true')
  const endpoints = document.createElement('div')
  endpoints.className = 'traffic-endpoints'
  endpoints.append(source, directions, target)
  const latency = document.createElement('div')
  latency.className = 'globe-flow-latency'
  const clock = document.createElement('i')
  clock.className = 'pi pi-clock'
  clock.setAttribute('aria-hidden', 'true')
  const latencyValue = document.createElement('span')
  latencyValue.className = 'globe-flow-latency-value'
  latency.append(clock, latencyValue)
  cross.append(top, endpoints, bottom, latency)
  updateFlowLabel(cross, input)
  return cross
}
