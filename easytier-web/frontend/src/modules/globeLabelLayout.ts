export interface ScreenPoint {
  x: number
  y: number
}

export interface LabelRect extends ScreenPoint {
  width: number
  height: number
}

export interface LabelAnchor extends ScreenPoint {
  index: number
  tangent?: ScreenPoint
}

export interface LabelChoice {
  anchorIndex: number
  direction: number
}

export interface LayoutLabel {
  id: string
  kind: 'node' | 'traffic'
  width: number
  height: number
  priority: number
  anchors: LabelAnchor[]
  previous?: LabelChoice
  stacked?: boolean
  rows?: { width: number, height: number }[]
}

export interface LabelPlacement extends LabelRect {
  id: string
  anchor: LabelAnchor
  leaderEnd: ScreenPoint
  leaderLength: number
  choice: LabelChoice
  labelCount: number
}

export const MAX_TRAFFIC_LEADER_LENGTH = 48
export const MAX_NODE_LEADER_LENGTH = 32
export const LABEL_STACK_GAP = 4
const LABEL_GAP = 12
const COLLISION_PADDING = 4

const clamp = (value: number, minimum: number, maximum: number) =>
  Math.min(maximum, Math.max(minimum, value))

function overlaps(left: LabelRect, right: LabelRect): boolean {
  return left.x < right.x + right.width + COLLISION_PADDING
    && left.x + left.width + COLLISION_PADDING > right.x
    && left.y < right.y + right.height + COLLISION_PADDING
    && left.y + left.height + COLLISION_PADDING > right.y
}

/** Clip a short connector against the padded box instead of checking endpoints alone. */
function crossesBox(start: ScreenPoint, end: ScreenPoint, box: LabelRect): boolean {
  let entry = 0
  let exit = 1
  for (const [position, delta, minimum, maximum] of [
    [start.x, end.x - start.x, box.x - COLLISION_PADDING, box.x + box.width + COLLISION_PADDING],
    [start.y, end.y - start.y, box.y - COLLISION_PADDING, box.y + box.height + COLLISION_PADDING],
  ]) {
    if (Math.abs(delta) < 1e-9) {
      if (position < minimum || position > maximum)
        return false
      continue
    }
    const first = (minimum - position) / delta
    const last = (maximum - position) / delta
    entry = Math.max(entry, Math.min(first, last))
    exit = Math.min(exit, Math.max(first, last))
    if (entry > exit)
      return false
  }
  return true
}

function candidates(label: LayoutLabel, width: number, height: number): {
  placement: LabelPlacement
  score: number
}[] {
  if (label.rows?.length) {
    let stackWidth = 0
    let stackHeight = 0
    return label.rows.flatMap((row, index) => {
      stackWidth = Math.max(stackWidth, row.width)
      stackHeight += row.height + (index ? LABEL_STACK_GAP : 0)
      return candidates({ ...label, width: stackWidth, height: stackHeight, rows: undefined }, width, height)
        .map(candidate => ({
          placement: { ...candidate.placement, labelCount: index + 1 },
          score: candidate.score + (label.rows!.length - index - 1) * 40,
        }))
    })
      .sort((left, right) => left.score - right.score)
  }
  const result: { placement: LabelPlacement, score: number }[] = []
  const right = width - label.width - 5
  const bottom = height - label.height - 34
  if (![label.width, label.height, right, bottom].every(Number.isFinite)
    || label.width <= 0 || label.height <= 0 || right < 5 || bottom < 5)
    return result
  const maxLeader = label.kind === 'traffic' ? MAX_TRAFFIC_LEADER_LENGTH : MAX_NODE_LEADER_LENGTH
  for (const anchor of label.anchors) {
    if (![anchor.x, anchor.y].every(Number.isFinite) || anchor.x < 0 || anchor.x > width
      || anchor.y < 0 || anchor.y > height)
      continue
    const tangentLength = anchor.tangent ? Math.hypot(anchor.tangent.x, anchor.tangent.y) : 0
    const tangent = tangentLength > 1e-6 && Number.isFinite(tangentLength)
      ? { x: anchor.tangent!.x / tangentLength, y: anchor.tangent!.y / tangentLength } : undefined
    const normal = tangent ? { x: -tangent.y, y: tangent.x } : { x: 0, y: -1 }
    const directions = label.kind === 'traffic' && !label.stacked
      ? [normal, { x: -normal.x, y: -normal.y }, { x: 0, y: -1 }, { x: 0, y: 1 }, { x: 1, y: 0 }, { x: -1, y: 0 }]
      : label.kind === 'node' && label.stacked
        ? [{ x: 0, y: -1 }, { x: 0, y: 1 }, { x: 1, y: 0 }, { x: -1, y: 0 }]
        : [{ x: 1, y: 0 }, { x: -1, y: 0 }, { x: 0, y: -1 }, { x: 0, y: 1 }]
    directions.forEach((direction, side) => {
      for (const [gapIndex, gap] of (label.stacked ? [LABEL_GAP, 20, 28] : [LABEL_GAP]).entries()) {
        for (const corner of [false, true]) {
          const distance = Math.abs(direction.x) * label.width / 2
            + Math.abs(direction.y) * label.height / 2 + gap
          // Corner attachment keeps a diagonal link's connector short even for a wide box.
          const desiredX = corner
            ? anchor.x + direction.x * gap - (Math.abs(direction.x) < 0.15 ? label.width / 2 : direction.x < 0 ? label.width : 0)
            : anchor.x + direction.x * distance - label.width / 2
          const desiredY = corner
            ? anchor.y + direction.y * gap - (Math.abs(direction.y) < 0.15 ? label.height / 2 : direction.y < 0 ? label.height : 0)
            : anchor.y + direction.y * distance - label.height / 2
          const x = clamp(desiredX, 5, right)
          const y = clamp(desiredY, 5, bottom)
          const rect = { x, y, width: label.width, height: label.height }
          const leaderEnd = {
            x: clamp(anchor.x, x, x + label.width),
            y: clamp(anchor.y, y, y + label.height),
          }
          const leaderLength = Math.hypot(leaderEnd.x - anchor.x, leaderEnd.y - anchor.y)
          if (leaderLength < 3 || leaderLength > maxLeader)
            continue
          if (label.kind === 'traffic' && tangent && !label.stacked) {
            const span = Math.hypot(label.width, label.height) + maxLeader
            if (crossesBox(
              { x: anchor.x - tangent.x * span, y: anchor.y - tangent.y * span },
              { x: anchor.x + tangent.x * span, y: anchor.y + tangent.y * span },
              rect,
            ))
              continue
          }
          const choice = {
            anchorIndex: anchor.index,
            direction: side * (label.stacked ? 6 : 2) + gapIndex * 2 + Number(corner),
          }
          const unchanged = label.previous?.anchorIndex === choice.anchorIndex
            && label.previous.direction === choice.direction
          result.push({
            placement: { id: label.id, ...rect, anchor, leaderEnd, leaderLength, choice, labelCount: 1 },
            score: leaderLength + anchor.index * 2 + side * 2 + Number(corner) * 3
              + Math.hypot(x - desiredX, y - desiredY) * 0.25 - (unchanged ? 24 : 0),
          })
        }
      }
    })
  }
  return result.sort((left, right) => left.score - right.score)
}

/** Place locally along each link, never pushing a crowded label into a distant row. */
export function arrangeGlobeLabels(
  labels: LayoutLabel[],
  width: number,
  height: number,
  reserved: LabelRect[] = [],
): LabelPlacement[] {
  const placed: LabelPlacement[] = []
  const seen = new Set<string>()
  for (const label of [...labels].sort((left, right) => right.priority - left.priority || left.id.localeCompare(right.id))) {
    if (seen.has(label.id))
      continue
    seen.add(label.id)
    for (const { placement } of candidates(label, width, height)) {
      if (reserved.some(rect => overlaps(placement, rect))
        || placed.some(previous => overlaps(placement, previous)
          || crossesBox(placement.anchor, placement.leaderEnd, previous)
          || crossesBox(previous.anchor, previous.leaderEnd, placement)))
        continue
      placed.push(placement)
      break
    }
  }
  return placed
}
