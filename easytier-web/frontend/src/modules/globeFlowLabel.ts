export interface FlowLabelInput {
  sourceLabel: string
  targetLabel: string
  txBps?: number
  rxBps?: number
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

export function createFlowLabel(input: FlowLabelInput): HTMLDivElement {
  const cross = document.createElement('div')
  cross.className = 'globe-flow-cross'
  const forward = `${input.sourceLabel} \u2192 ${input.targetLabel}`
  const reverse = `${input.targetLabel} \u2192 ${input.sourceLabel}`
  const sourceStats = formatRate(input.txBps)
  const targetStats = formatRate(input.rxBps)
  cross.setAttribute('aria-label', `${forward}: ${sourceStats}; ${reverse}: ${targetStats}`)

  const top = document.createElement('div')
  top.className = 'globe-flow-stat globe-flow-source-stat'
  top.textContent = sourceStats
  top.title = `${forward}: ${sourceStats}`
  const bottom = document.createElement('div')
  bottom.className = 'globe-flow-stat globe-flow-target-stat'
  bottom.textContent = targetStats
  bottom.title = `${reverse}: ${targetStats}`
  const source = document.createElement('span')
  source.className = 'globe-flow-name globe-flow-source'
  source.textContent = input.sourceLabel
  source.title = input.sourceLabel
  const target = document.createElement('span')
  target.className = 'globe-flow-name globe-flow-target'
  target.textContent = input.targetLabel
  target.title = input.targetLabel
  const directions = document.createElement('i')
  directions.className = 'pi pi-arrow-right-arrow-left globe-flow-directions'
  directions.setAttribute('aria-hidden', 'true')
  const endpoints = document.createElement('div')
  endpoints.className = 'traffic-endpoints'
  endpoints.append(source, directions, target)
  cross.append(top, endpoints, bottom)
  return cross
}
