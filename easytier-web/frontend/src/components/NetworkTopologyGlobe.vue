<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { Button } from 'primevue'
import { useI18n } from 'vue-i18n'
import { geoEquirectangular, geoPath } from 'd3-geo'
import * as THREE from 'three'
import { OrbitControls } from 'three/addons/controls/OrbitControls.js'
import { locateNode, worldGeography, type LocatedNode } from '../modules/globeGeography'
import { spherePosition as position, sphericalArc } from '../modules/globeBoundaryGeometry'
import { FlowEmitter, flowEmissionsPerSecond, flowTravelSeconds, MAX_FLOW_PARTICLES } from '../modules/globeFlow'
import { createFlowLabel, updateFlowLabel, type FlowLabelInput } from '../modules/globeFlowLabel'
import { arrangeGlobeLabels, type LabelChoice, type LayoutLabel } from '../modules/globeLabelLayout'
import { linkLocationGroup, nodeLocationGroup } from '../modules/globeLabelGroups'
import { loadGlobeMapDetail } from '../modules/globeMapDetail'
import { buildCloudPointPositions } from '../modules/globePointCloud'
import { readGlobePreferences, saveGlobePreferences } from '../modules/dashboardPersistence'
import type { TopologyLink, TopologyNode } from '../modules/networkTopology'

const props = defineProps<{
  nodes: TopologyNode[]
  links: TopologyLink[]
  loading?: boolean
  persistenceKey?: string
}>()
const emit = defineEmits<{ refresh: [] }>()
const { t } = useI18n()
const stage = ref<HTMLDivElement>()
const labelLayer = ref<HTMLDivElement>()
const stageHeight = ref(360)
const unavailable = ref(false)
const rotating = ref(true)
const selectedId = ref('')
const locatedNodes = computed(() => props.nodes.flatMap(node => {
  const located = locateNode(node)
  return located ? [located] : []
}))
const selectedNode = computed(() => props.nodes.find(node => node.id === selectedId.value))
const selectedLocation = computed(() => selectedNode.value && locateNode(selectedNode.value))
const unmappedCount = computed(() => props.nodes.length - locatedNodes.value.length)

let scene: THREE.Scene
let camera: THREE.PerspectiveCamera
let renderer: THREE.WebGLRenderer | undefined
let controls: OrbitControls | undefined
let resizeObserver: ResizeObserver | undefined
let themeObserver: MutationObserver | undefined
let topologyGroup: THREE.Group | undefined
let globeSurface: THREE.Mesh | undefined
let boundaryMaterial: THREE.LineBasicMaterial | undefined
const cloudLevels: { group?: THREE.Group, maxDistance: number, count: number, angularStep: number }[] = [
  { maxDistance: Number.POSITIVE_INFINITY, count: 24_000, angularStep: 0.045 },
  { maxDistance: 2.45, count: 96_000, angularStep: 0.012 },
  { maxDistance: 1.55, count: 288_000, angularStep: 0.003 },
]
const boundaryLevels: (THREE.LineSegments | undefined)[] = []
const detailRequests: (Promise<void> | undefined)[] = []
const detailRetryAt: number[] = []
let disposed = false
let restoringView = false
let viewSaveTimer: ReturnType<typeof setTimeout> | undefined
let lastViewSavedAt = 0
let lastViewDistance = 0
let activeCloudLevel = -1
let activeBoundaryLevel = -1
let frame = 0
let lastFrame = 0
let interacting = false
let interactionStart: { x: number, y: number } | undefined
let dragged = false
let markerMeshes: THREE.Mesh[] = []
let flows: {
  key: string
  mesh: THREE.InstancedMesh
  curve: THREE.CatmullRomCurve3
  emitter: FlowEmitter
  emissionsPerSecond: number
  travelSeconds: number
  reverse: boolean
}[] = []
interface GlobeLabel {
  id: string
  kind: 'node' | 'traffic'
  element: HTMLDivElement
  stackId: string
  anchors: { point: THREE.Vector3, tangentPoints?: [THREE.Vector3, THREE.Vector3] }[]
  priority: number
  size?: { width: number, height: number }
}
interface GlobeLabelStack {
  id: string
  kind: 'node' | 'traffic'
  element: HTMLDivElement
  leader: HTMLDivElement
  labels: GlobeLabel[]
  choice?: LabelChoice
}
const labels = new Map<string, GlobeLabel>()
const labelStacks = new Map<string, GlobeLabelStack>()
const raycaster = new THREE.Raycaster()
const pointer = new THREE.Vector2()
const markerViewPosition = new THREE.Vector3()
const flowMarker = new THREE.Object3D()
const labelProjection = new THREE.Vector3()
const labelTangentStart = new THREE.Vector3()
const labelTangentEnd = new THREE.Vector3()
const labelDirection = new THREE.Vector3()
const labelIntersection = new THREE.Vector3()
const labelRay = new THREE.Ray()
const globeOccluder = new THREE.Sphere(new THREE.Vector3(), 0.994)
const NODE_LABEL_MAX_DISTANCE = 2.8
const TRAFFIC_LABEL_MAX_DISTANCE = 1.7
const controlsConfig = {
  rotateSpeed: 0.26,
  zoomSpeed: 0.78,
  minDistance: 1.25,
  maxDistance: 6.5,
  autoRotateSpeed: 0.21,
}

const globeColorRoles = ['surface', 'land', 'ocean', 'boundary', 'managed', 'peer', 'link', 'flow-forward', 'flow-reverse'] as const
type GlobeColorRole = typeof globeColorRoles[number]
type GlobeMaterial = THREE.Material & { color: THREE.Color }
let globeColors: Record<GlobeColorRole, string>

function themedMaterial<T extends GlobeMaterial>(material: T, role: GlobeColorRole): T {
  material.userData.globeColorRole = role
  material.color.set(globeColors[role])
  return material
}

function syncTheme() {
  if (!stage.value)
    return
  // Share CSS colors with WebGL, including geometry loaded after a theme change.
  const style = getComputedStyle(stage.value)
  globeColors = Object.fromEntries(globeColorRoles.map(role =>
    [role, style.getPropertyValue(`--globe-${role}`).trim()])) as Record<GlobeColorRole, string>
  // Recolor in place to preserve camera, selection, particles and map detail.
  scene?.traverse(object => {
    const material = (object as THREE.Mesh).material
    for (const item of Array.isArray(material) ? material : material ? [material] : []) {
      const role = item.userData.globeColorRole as GlobeColorRole | undefined
      if (role)
        (item as GlobeMaterial).color.set(globeColors[role])
    }
  })
}

function disposeGroup(group: THREE.Object3D) {
  group.traverse(object => {
    const renderable = object as THREE.Mesh
    renderable.geometry?.dispose()
    if (Array.isArray(renderable.material))
      renderable.material.forEach(material => material.dispose())
    else
      renderable.material?.dispose()
    if (object instanceof THREE.InstancedMesh)
      object.dispose()
  })
}

function buildCountryBoundaries(geography: typeof worldGeography, angularStep: number) {
  const features = (geography as unknown as {
    features?: { geometry?: { type?: string, coordinates?: unknown } }[]
  }).features ?? []
  const points: number[] = []
  let sourceVertices = 0
  for (const feature of features) {
    const geometry = feature.geometry
    if (!geometry?.coordinates)
      continue
    const polygons = geometry.type === 'Polygon'
      ? [geometry.coordinates]
      : geometry.type === 'MultiPolygon' ? geometry.coordinates : []
    for (const polygon of polygons as unknown[]) {
      if (!Array.isArray(polygon))
        continue
      for (const ring of polygon as unknown[]) {
        if (!Array.isArray(ring))
          continue
        const coordinates = ring.filter((coordinate): coordinate is [number, number] =>
          Array.isArray(coordinate)
          && typeof coordinate[0] === 'number'
          && typeof coordinate[1] === 'number'
          && Number.isFinite(coordinate[0])
          && Number.isFinite(coordinate[1]))
        if (coordinates.length < 2)
          continue
        sourceVertices += coordinates.length
        for (let index = 0; index < coordinates.length - 1; index++) {
          const [sourceLongitude, sourceLatitude] = coordinates[index]
          const [targetLongitude, targetLatitude] = coordinates[index + 1]
          const arc = sphericalArc(
            position(sourceLatitude, sourceLongitude),
            position(targetLatitude, targetLongitude),
            1.003,
            angularStep,
          )
          for (let segment = 0; segment < arc.length - 1; segment++) {
            const from = arc[segment]
            const to = arc[segment + 1]
            points.push(from.x, from.y, from.z, to.x, to.y, to.z)
          }
        }
      }
    }
  }
  const geometry = new THREE.BufferGeometry()
  geometry.setAttribute('position', new THREE.Float32BufferAttribute(points, 3))
  const lines = new THREE.LineSegments(geometry, boundaryMaterial)
  lines.userData.sourceVertices = sourceVertices
  lines.userData.angularStep = angularStep
  return lines
}

function buildPointCloud(
  map: HTMLCanvasElement,
  pixels: Uint8ClampedArray,
  count: number,
) {
  const { land, ocean } = buildCloudPointPositions(count, (lat, lon) => {
    const x = Math.min(map.width - 1, Math.floor((lon + 180) / 360 * map.width))
    const y = Math.min(map.height - 1, Math.floor((90 - lat) / 180 * map.height))
    return pixels[(y * map.width + x) * 4 + 3] > 100
  })
  const group = new THREE.Group()
  for (const [points, role, size] of [
    [land, 'land', count > 100_000 ? 0.0035 : count > 30_000 ? 0.006 : 0.011],
    [ocean, 'ocean', count > 100_000 ? 0.002 : count > 30_000 ? 0.0035 : 0.006],
  ] as const) {
    const geometry = new THREE.BufferGeometry()
    geometry.setAttribute('position', new THREE.Float32BufferAttribute(points, 3))
    group.add(new THREE.Points(geometry, themedMaterial(new THREE.PointsMaterial({
      size,
      sizeAttenuation: true,
    }), role)))
  }
  return group
}

function buildGeographyLevel(level: number, geography: typeof worldGeography) {
  const map = document.createElement('canvas')
  map.width = 2048
  map.height = 1024
  const context = map.getContext('2d')
  if (!context)
    return
  const projection = geoEquirectangular().scale(map.width / (2 * Math.PI))
    .translate([map.width / 2, map.height / 2])
  context.fillStyle = '#ffffff'
  context.beginPath()
  geoPath(projection, context)(geography)
  context.fill()
  const pixels = context.getImageData(0, 0, map.width, map.height).data
  const config = cloudLevels[level]
  const cloud = buildPointCloud(map, pixels, config.count)
  cloud.visible = false
  config.group = cloud
  const boundaries = buildCountryBoundaries(geography, config.angularStep)
  boundaries.visible = false
  boundaryLevels[level] = boundaries
  scene.add(cloud, boundaries)
}

function requestGeographyLevel(level: 1 | 2) {
  if (cloudLevels[level].group || detailRequests[level] || performance.now() < (detailRetryAt[level] ?? 0))
    return
  detailRequests[level] = loadGlobeMapDetail(level)
    .then(geography => {
      if (!disposed)
        buildGeographyLevel(level, geography)
    })
    .catch(error => {
      detailRetryAt[level] = performance.now() + 30_000
      console.warn('Failed to load globe geography detail', error)
    })
    .finally(() => {
      detailRequests[level] = undefined
    })
}

function buildCloud() {
  boundaryMaterial = themedMaterial(new THREE.LineBasicMaterial({
    transparent: true,
    opacity: 0.38,
    depthWrite: false,
  }), 'boundary')
  buildGeographyLevel(0, worldGeography)
  globeSurface = new THREE.Mesh(
    new THREE.SphereGeometry(0.994, 64, 32),
    themedMaterial(new THREE.MeshBasicMaterial(), 'surface'),
  )
  scene.add(globeSurface)
}

function updateCloudDetail() {
  if (!camera || !controls)
    return
  const distance = camera.position.distanceTo(controls.target)
  // Match the projected front-surface displacement to pointer pixels, even
  // when zoomed in, instead of applying a fixed angular drag multiplier.
  controls.rotateSpeed = THREE.MathUtils.clamp(
    (distance - 1) * Math.tan(THREE.MathUtils.degToRad(camera.fov / 2)) / Math.PI,
    0.025,
    0.6,
  )
  const requestedLevel = distance <= cloudLevels[2].maxDistance ? 2
    : distance <= cloudLevels[1].maxDistance ? 1 : 0
  if (distance <= NODE_LABEL_MAX_DISTANCE)
    requestGeographyLevel(1)
  if (distance <= 1.9)
    requestGeographyLevel(2)
  let nextLevel = requestedLevel
  while (nextLevel > 0 && !cloudLevels[nextLevel].group)
    nextLevel--
  if (nextLevel !== activeCloudLevel) {
    cloudLevels.forEach(({ group }, index) => {
      if (group)
        group.visible = index === nextLevel
    })
    activeCloudLevel = nextLevel
  }
  if (nextLevel !== activeBoundaryLevel) {
    boundaryLevels.forEach((lines, index) => {
      if (lines)
        lines.visible = index === nextLevel
    })
    activeBoundaryLevel = nextLevel
  }
  if (renderer) {
    renderer.domElement.dataset.globeRotateSpeed = String(controls.rotateSpeed)
    renderer.domElement.dataset.globeDistance = String(distance)
    renderer.domElement.dataset.globeCloudLevel = String(nextLevel)
    renderer.domElement.dataset.globeBoundaryLevel = String(nextLevel)
    renderer.domElement.dataset.globeBoundaryScale = ['110m', '50m', '10m'][nextLevel]
    renderer.domElement.dataset.globeBoundarySourceVertices = String(boundaryLevels[nextLevel]?.userData.sourceVertices ?? 0)
    renderer.domElement.dataset.globeBoundaryAngularStep = String(cloudLevels[nextLevel].angularStep)
    renderer.domElement.dataset.globeAzimuth = String(controls.getAzimuthalAngle())
  }
  if (boundaryMaterial) {
    const zoom = THREE.MathUtils.clamp((5 - distance) / 3.75, 0, 1)
    boundaryMaterial.opacity = THREE.MathUtils.lerp(0.35, 0.72, zoom)
  }
}

function scaleMarker(mesh: THREE.Object3D, radius: number, pixels: number) {
  if (!renderer)
    return
  markerViewPosition.copy(mesh.position).applyMatrix4(camera.matrixWorldInverse)
  const unitsPerPixel = -markerViewPosition.z
    * 2 * Math.tan(THREE.MathUtils.degToRad(camera.fov / 2))
    / Math.max(1, renderer.domElement.clientHeight)
  mesh.scale.setScalar(Math.max(0.001, unitsPerPixel * pixels / radius))
}

function addLabel(id: string, content: string | FlowLabelInput, kind: 'node' | 'traffic', anchors: GlobeLabel['anchors'], priority: number, stale?: boolean, stackId = id) {
  if (!labelLayer.value)
    return
  let label = labels.get(id)
  if (!label) {
    const element = document.createElement('div')
    element.dataset.labelId = id
    element.style.display = 'none'
    label = { id, kind, element, stackId, anchors, priority }
    labels.set(id, label)
  }
  const { element } = label
  element.className = `globe-${kind}-label${stale ? ' is-stale' : ''}`
  label.anchors = anchors
  label.priority = priority
  label.stackId = stackId
  element.dataset.labelGroup = stackId
  if (typeof content === 'string') {
    if (element.textContent !== content) {
      element.textContent = content
      label.size = undefined
    }
  } else {
    const cross = element.firstElementChild as HTMLDivElement | null
    if (cross)
      updateFlowLabel(cross, content)
    else
      element.append(createFlowLabel(content))
    element.setAttribute('role', 'img')
    element.setAttribute('aria-label', element.firstElementChild!.getAttribute('aria-label') ?? '')
  }
}

function syncLabelStacks() {
  if (!labelLayer.value)
    return
  labelStacks.forEach(stack => { stack.labels = [] })
  for (const label of [...labels.values()].sort((left, right) => left.id.localeCompare(right.id))) {
    let stack = labelStacks.get(label.stackId)
    if (!stack) {
      const element = document.createElement('div')
      element.className = `globe-label-stack globe-${label.kind}-stack`
      element.dataset.stackId = label.stackId
      element.style.display = 'none'
      element.addEventListener('wheel', event => event.stopPropagation())
      element.addEventListener('pointerdown', event => event.stopPropagation())
      const leader = document.createElement('div')
      leader.className = `globe-label-leader globe-${label.kind}-leader`
      leader.style.display = 'none'
      labelLayer.value.append(leader, element)
      stack = { id: label.stackId, kind: label.kind, element, leader, labels: [] }
      labelStacks.set(label.stackId, stack)
    }
    const index = stack.labels.length
    stack.labels.push(label)
    if (stack.element.children[index] !== label.element)
      stack.element.insertBefore(label.element, stack.element.children[index] ?? null)
  }
  for (const [id, stack] of labelStacks) {
    if (!stack.labels.length) {
      stack.element.remove()
      stack.leader.remove()
      labelStacks.delete(id)
    }
  }
}

function projectedAnchor(point: THREE.Vector3, width: number, height: number) {
  labelProjection.copy(point).project(camera)
  if (Math.abs(labelProjection.x) > 1 || Math.abs(labelProjection.y) > 1
    || Math.abs(labelProjection.z) > 1)
    return undefined
  labelDirection.copy(point).sub(camera.position).normalize()
  labelRay.set(camera.position, labelDirection)
  const hit = labelRay.intersectSphere(globeOccluder, labelIntersection)
  if (hit && hit.distanceToSquared(camera.position) + 0.001 < point.distanceToSquared(camera.position))
    return undefined
  return {
    x: (labelProjection.x + 1) * width / 2,
    y: (1 - labelProjection.y) * height / 2,
  }
}

function updateLabels() {
  if (!renderer || !controls || !labelLayer.value)
    return
  const distance = camera.position.distanceTo(controls.target)
  const nodesVisible = distance <= NODE_LABEL_MAX_DISTANCE
  const trafficVisible = distance <= TRAFFIC_LABEL_MAX_DISTANCE
  // Aspect-ratio layouts can have fractional sizes; rounded clientHeight lets
  // bottom-aligned labels encroach on the legend after a viewport resize.
  const { width, height } = renderer.domElement.getBoundingClientRect()
  labelLayer.value.dataset.detailVisible = String(nodesVisible)
  labelLayer.value.dataset.nodeLabelsVisible = String(nodesVisible)
  labelLayer.value.dataset.trafficLabelsVisible = String(trafficVisible)
  const pending: LayoutLabel[] = []
  for (const stack of labelStacks.values()) {
    if (!nodesVisible || (stack.kind === 'traffic' && !trafficVisible))
      continue
    const anchors = stack.labels[0].anchors.flatMap(({ point, tangentPoints }, index) => {
      const anchor = projectedAnchor(point, width, height)
      if (!anchor)
        return []
      let tangent
      if (tangentPoints) {
        labelTangentStart.copy(tangentPoints[0]).project(camera)
        labelTangentEnd.copy(tangentPoints[1]).project(camera)
        tangent = {
          x: (labelTangentEnd.x - labelTangentStart.x) * width / 2,
          y: -(labelTangentEnd.y - labelTangentStart.y) * height / 2,
        }
      }
      return [{ ...anchor, index, tangent }]
    })
    if (!anchors.length)
      continue
    const previousDisplay = stack.element.style.display
    stack.element.style.display = 'flex'
    for (const label of stack.labels) {
      label.element.style.display = 'block'
      if (!label.size)
        label.size = { width: label.element.offsetWidth, height: label.element.offsetHeight }
    }
    const stackWidth = Math.max(...stack.labels.map(label => label.size!.width)) + 8
    let stackHeight = 0
    const heights = stack.labels.flatMap((label, index) => {
      stackHeight += label.size!.height + (index ? 4 : 0)
      return stackHeight <= height - 39 ? [stackHeight] : []
    }).reverse()
    stack.element.style.width = `${stackWidth}px`
    stack.element.style.display = previousDisplay
    if (!heights.length)
      continue
    pending.push({
      id: stack.id, kind: stack.kind, anchors,
      priority: Math.max(...stack.labels.map(label => label.priority)),
      width: stackWidth, height: heights[0], previous: stack.choice,
      stacked: stack.labels.length > 1,
      heights: stack.labels.length > 1 ? heights : undefined,
    })
  }
  const markers = markerMeshes.flatMap(mesh => {
    const anchor = projectedAnchor(mesh.position, width, height)
    return anchor ? [{ x: anchor.x - 7, y: anchor.y - 7, width: 14, height: 14 }] : []
  })
  const visibleIds = new Set<string>()
  for (const placement of arrangeGlobeLabels(pending, width, height, markers)) {
    const stack = labelStacks.get(placement.id)!
    visibleIds.add(placement.id)
    stack.choice = placement.choice
    stack.element.style.display = 'flex'
    stack.element.style.left = `${placement.x}px`
    stack.element.style.top = `${placement.y}px`
    stack.element.style.height = `${placement.height}px`
    stack.leader.style.display = 'block'
    stack.leader.style.left = `${placement.anchor.x}px`
    stack.leader.style.top = `${placement.anchor.y}px`
    stack.leader.style.width = `${placement.leaderLength}px`
    stack.leader.style.transform = `rotate(${Math.atan2(
      placement.leaderEnd.y - placement.anchor.y, placement.leaderEnd.x - placement.anchor.x,
    )}rad)`
  }
  for (const stack of labelStacks.values()) {
    if (!visibleIds.has(stack.id)) {
      stack.element.style.display = 'none'
      stack.leader.style.display = 'none'
      stack.labels.forEach(label => { label.element.style.display = 'none' })
    }
  }
}

function beginInteraction(event: PointerEvent) {
  interacting = true
  interactionStart = { x: event.clientX, y: event.clientY }
  dragged = false
  if (controls)
    controls.autoRotate = false
}

function moveInteraction(event: PointerEvent) {
  if (interacting && interactionStart
    && Math.hypot(event.clientX - interactionStart.x, event.clientY - interactionStart.y) > 4)
    dragged = true
}

function endInteraction() {
  const wasInteracting = interacting
  interacting = false
  if (wasInteracting)
    saveView()
}

function linkCurve(source: THREE.Vector3, target: THREE.Vector3) {
  const angle = source.angleTo(target)
  if (angle < 0.0001) {
    // Country-only locations can coincide. Use a visible loop rather than
    // a zero-length curve, whose arc-length sampling produces NaN positions.
    const normal = source.clone().normalize()
    const tangent = new THREE.Vector3().crossVectors(normal, new THREE.Vector3(0, 1, 0))
    if (tangent.lengthSq() < 0.00001)
      tangent.set(1, 0, 0)
    tangent.normalize()
    const side = new THREE.Vector3().crossVectors(normal, tangent).normalize()
    const points = Array.from({ length: 49 }, (_, i) => {
      const phase = i / 48 * Math.PI * 2
      return normal.clone().multiplyScalar(1.018 + Math.sin(phase / 2) * 0.04)
        .addScaledVector(tangent, Math.sin(phase) * 0.04)
        .addScaledVector(side, (1 - Math.cos(phase)) * 0.025)
    })
    return new THREE.CatmullRomCurve3(points)
  }
  let axis = new THREE.Vector3().crossVectors(source, target)
  if (axis.lengthSq() < 0.00001)
    axis = new THREE.Vector3().crossVectors(source, new THREE.Vector3(0, 1, 0))
  if (axis.lengthSq() < 0.00001)
    axis = new THREE.Vector3(1, 0, 0)
  axis.normalize()
  const points = Array.from({ length: 49 }, (_, i) => {
    const progress = i / 48
    return source.clone().normalize().applyAxisAngle(axis, angle * progress)
      .multiplyScalar(1.018 + Math.sin(progress * Math.PI) * Math.min(0.3, angle * 0.15))
  })
  return new THREE.CatmullRomCurve3(points)
}

function rebuildTopology() {
  if (!renderer)
    return
  const previousEmitters = new Map(flows.map(flow => [flow.key, flow.emitter]))
  const activeLabelIds = new Set<string>()
  if (topologyGroup) {
    scene.remove(topologyGroup)
    disposeGroup(topologyGroup)
  }
  topologyGroup = new THREE.Group()
  markerMeshes = []
  flows = []
  const nodeMap = new Map<string, LocatedNode>(locatedNodes.value.map(node => [node.id, node]))
  const nodeGroups = new Map([...nodeMap.values()].map(node => [node.id, nodeLocationGroup(node)]))
  const groupPositions = new Map<string, THREE.Vector3>()
  for (const node of nodeMap.values()) {
    const groupId = nodeGroups.get(node.id)!
    const center = groupPositions.get(groupId) ?? new THREE.Vector3()
    center.add(position(node.latitude, node.longitude))
    groupPositions.set(groupId, center)
  }
  groupPositions.forEach(center => center.normalize())
  const labelCurves = new Map<string, THREE.CatmullRomCurve3>()
  for (const node of nodeMap.values()) {
    const mesh = new THREE.Mesh(
      new THREE.SphereGeometry(node.id === selectedId.value ? 0.025 : 0.018, 12, 8),
      themedMaterial(new THREE.MeshBasicMaterial({
        transparent: !!node.stale,
        opacity: node.stale ? 0.5 : 1,
      }), node.managed ? 'managed' : 'peer'),
    )
    mesh.position.copy(position(node.latitude, node.longitude, 1.018))
    mesh.userData.nodeId = node.id
    mesh.userData.radius = node.id === selectedId.value ? 0.025 : 0.018
    markerMeshes.push(mesh)
    topologyGroup.add(mesh)
    const labelId = `node:${node.id}`
    activeLabelIds.add(labelId)
    addLabel(labelId, node.label, 'node',
      [{ point: groupPositions.get(nodeGroups.get(node.id)!)!.clone().multiplyScalar(1.018) }],
      node.id === selectedId.value ? 10 : 6, node.stale, `node:${nodeGroups.get(node.id)}`)
  }
  for (const [index, link] of props.links.entries()) {
    const source = nodeMap.get(link.source)
    const target = nodeMap.get(link.target)
    if (!source || !target)
      continue
    const curve = linkCurve(position(source.latitude, source.longitude), position(target.latitude, target.longitude))
    topologyGroup.add(new THREE.Line(
      new THREE.BufferGeometry().setFromPoints(curve.getPoints(64)),
      themedMaterial(new THREE.LineBasicMaterial({ transparent: true, opacity: link.stale ? 0.28 : 0.72 }), 'link'),
    ))
    for (const [reverse, rate] of [[false, link.txBps], [true, link.rxBps]] as const) {
      const emissionsPerSecond = flowEmissionsPerSecond(rate, link.stale)
      if (!emissionsPerSecond)
        continue
      const travelSeconds = flowTravelSeconds(link.latencyMs)
      const mesh = new THREE.InstancedMesh(
        new THREE.SphereGeometry(0.009, 8, 6),
        themedMaterial(new THREE.MeshBasicMaterial(), reverse ? 'flow-reverse' : 'flow-forward'),
        MAX_FLOW_PARTICLES,
      )
      mesh.count = 0
      mesh.frustumCulled = false
      mesh.instanceMatrix.setUsage(THREE.DynamicDrawUsage)
      const key = JSON.stringify([link.source, link.target, reverse])
      const emitter = previousEmitters.get(key) ?? new FlowEmitter((index * 0.23 + (reverse ? 0.37 : 0)) % 1)
      flows.push({ key, mesh, curve, emitter, emissionsPerSecond, travelSeconds, reverse })
      topologyGroup.add(mesh)
    }
    const labelId = `traffic:${JSON.stringify([link.source, link.target])}`
    const sourceGroup = nodeGroups.get(link.source)!
    const targetGroup = nodeGroups.get(link.target)!
    const groupId = linkLocationGroup(sourceGroup, targetGroup)
    let labelCurve = labelCurves.get(groupId)
    if (!labelCurve) {
      const [from, to] = [sourceGroup, targetGroup].sort()
      labelCurve = linkCurve(groupPositions.get(from)!, groupPositions.get(to)!)
      labelCurves.set(groupId, labelCurve)
    }
    activeLabelIds.add(labelId)
    addLabel(
      labelId,
      {
        sourceLabel: source.label,
        targetLabel: target.label,
        txBps: link.txBps,
        rxBps: link.rxBps,
        latencyMs: link.latencyMs,
      },
      'traffic',
      [0.5, 0.4, 0.6, 0.3, 0.7, 0.2, 0.8, 0.1, 0.9, 0.05, 0.95, 0.02, 0.98].map(progress => ({
        point: labelCurve!.getPoint(progress),
        tangentPoints: [labelCurve!.getPoint(Math.max(0, progress - 0.005)), labelCurve!.getPoint(Math.min(1, progress + 0.005))],
      })),
      link.source === selectedId.value || link.target === selectedId.value ? 7 : 1,
      link.stale,
      `traffic:${groupId}`,
    )
  }
  for (const [id, label] of labels) {
    if (!activeLabelIds.has(id)) {
      label.element.remove()
      labels.delete(id)
    }
  }
  syncLabelStacks()
  scene.add(topologyGroup)
  renderer.domElement.dataset.globeForwardEmissionRate = String(
    flows.filter(flow => !flow.reverse).reduce((total, flow) => total + flow.emissionsPerSecond, 0),
  )
  renderer.domElement.dataset.globeReverseEmissionRate = String(
    flows.filter(flow => flow.reverse).reduce((total, flow) => total + flow.emissionsPerSecond, 0),
  )
  renderer.domElement.dataset.globeFlowTimings = JSON.stringify(
    flows.map(flow => ({
      key: flow.key, emissionsPerSecond: flow.emissionsPerSecond, travelSeconds: flow.travelSeconds,
      emittedCount: flow.emitter.emittedCount,
    })),
  )
}

function selectNode(id: string) {
  if (selectedId.value === id) {
    selectedId.value = ''
    saveView()
    return
  }
  selectedId.value = id
  const node = locatedNodes.value.find(node => node.id === id)
  if (!node || !camera || !controls) {
    saveView()
    return
  }
  const distance = camera.position.distanceTo(controls.target)
  camera.position.copy(position(node.latitude, node.longitude, distance))
  controls.update()
  saveView()
}

function resetView(persist = true) {
  camera?.position.copy(position(22.3, 114.17, 3.3))
  controls?.target.set(0, 0, 0)
  controls?.update()
  if (persist)
    saveView()
}

function saveView() {
  if (!camera || !controls || disposed || restoringView)
    return
  clearTimeout(viewSaveTimer)
  saveGlobePreferences(props.persistenceKey ?? 'default', {
    position: [camera.position.x, camera.position.y, camera.position.z],
    rotating: rotating.value,
    selectedId: selectedId.value,
  })
  lastViewSavedAt = performance.now()
}

function restoreView() {
  if (!camera || !controls)
    return
  clearTimeout(viewSaveTimer)
  restoringView = true
  try {
    resetView(false)
    const saved = readGlobePreferences(props.persistenceKey ?? 'default')
    rotating.value = saved?.rotating ?? true
    selectedId.value = saved?.selectedId ?? ''
    if (saved)
      camera.position.fromArray(saved.position)
    controls.autoRotate = rotating.value && !interacting
    controls.update()
    lastViewDistance = camera.position.distanceTo(controls.target)
    lastViewSavedAt = performance.now()
  } finally {
    restoringView = false
  }
}

function scheduleZoomSave() {
  if (!camera || !controls || restoringView)
    return
  const distance = camera.position.distanceTo(controls.target)
  if (Math.abs(distance - lastViewDistance) <= 1e-6)
    return
  lastViewDistance = distance
  clearTimeout(viewSaveTimer)
  viewSaveTimer = setTimeout(saveView, 350)
}

function pickNode(event: MouseEvent) {
  if (!renderer || dragged)
    return
  const bounds = renderer.domElement.getBoundingClientRect()
  pointer.set((event.clientX - bounds.left) / bounds.width * 2 - 1,
    -(event.clientY - bounds.top) / bounds.height * 2 + 1)
  raycaster.setFromCamera(pointer, camera)
  const hit = raycaster.intersectObjects(globeSurface ? [globeSurface, ...markerMeshes] : markerMeshes)[0]
  if (hit?.object.userData.nodeId)
    selectNode(hit.object.userData.nodeId)
}

function animate(now: number) {
  if (!renderer || !controls)
    return
  if (now - lastFrame >= 1000 / 30) {
    controls.autoRotate = rotating.value && !interacting
    const deltaSeconds = lastFrame ? Math.min(0.1, (now - lastFrame) / 1000) : 1 / 30
    const flowSeconds = lastFrame ? (now - lastFrame) / 1000 : 0
    controls.update(deltaSeconds)
    updateCloudDetail()
    for (const mesh of markerMeshes)
      scaleMarker(mesh, mesh.userData.radius, mesh.userData.nodeId === selectedId.value ? 7 : 5)
    let invalidFlows = 0
    let forwardParticles = 0
    let reverseParticles = 0
    for (const flow of flows) {
      flow.emitter.advance(flowSeconds, flow.emissionsPerSecond, flow.travelSeconds)
      flow.mesh.count = flow.emitter.progress.length
      if (flow.reverse)
        reverseParticles += flow.mesh.count
      else
        forwardParticles += flow.mesh.count
      flow.emitter.progress.forEach((progress, index) => {
        flowMarker.position.copy(flow.curve.getPointAt(flow.reverse ? 1 - progress : progress))
        if (![flowMarker.position.x, flowMarker.position.y, flowMarker.position.z].every(Number.isFinite))
          invalidFlows++
        scaleMarker(flowMarker, 0.009, 1.6)
        flowMarker.updateMatrix()
        flow.mesh.setMatrixAt(index, flowMarker.matrix)
      })
      if (flow.mesh.count)
        flow.mesh.instanceMatrix.needsUpdate = true
    }
    renderer.domElement.dataset.globeForwardParticles = String(forwardParticles)
    renderer.domElement.dataset.globeReverseParticles = String(reverseParticles)
    renderer.domElement.dataset.globeInvalidFlows = String(invalidFlows)
    renderer.render(scene, camera)
    updateLabels()
    if (rotating.value && now - lastViewSavedAt >= 5_000)
      saveView()
    lastFrame = now
  }
  frame = requestAnimationFrame(animate)
}

onMounted(() => {
  if (!stage.value)
    return
  syncTheme()
  themeObserver = new MutationObserver(syncTheme)
  themeObserver.observe(document.documentElement, { attributes: true, attributeFilter: ['class'] })
  try {
    scene = new THREE.Scene()
    camera = new THREE.PerspectiveCamera(40, 1, 0.1, 20)
    renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true })
    renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2))
    renderer.domElement.setAttribute('aria-label', t('web.dashboard.topology'))
    renderer.domElement.addEventListener('click', pickNode)
    stage.value.appendChild(renderer.domElement)
    controls = new OrbitControls(camera, renderer.domElement)
    controls.enablePan = false
    controls.enableDamping = false
    controls.rotateSpeed = controlsConfig.rotateSpeed
    controls.zoomSpeed = controlsConfig.zoomSpeed
    controls.minDistance = controlsConfig.minDistance
    controls.maxDistance = controlsConfig.maxDistance
    controls.autoRotateSpeed = controlsConfig.autoRotateSpeed
    renderer.domElement.dataset.globeRotateSpeed = String(controlsConfig.rotateSpeed)
    renderer.domElement.dataset.globeZoomSpeed = String(controlsConfig.zoomSpeed)
    renderer.domElement.dataset.globeMinDistance = String(controlsConfig.minDistance)
    renderer.domElement.dataset.globeMaxDistance = String(controlsConfig.maxDistance)
    renderer.domElement.dataset.globeCloudLevels = '24000,96000,288000'
    renderer.domElement.dataset.globePauseOnInteraction = 'true'
    renderer.domElement.addEventListener('pointerdown', beginInteraction)
    renderer.domElement.addEventListener('pointermove', moveInteraction)
    window.addEventListener('pointerup', endInteraction)
    window.addEventListener('pointercancel', endInteraction)
    window.addEventListener('pagehide', saveView)
    restoreView()
    controls.addEventListener('change', scheduleZoomSave)
    buildCloud()
    rebuildTopology()
    resizeObserver = new ResizeObserver(() => {
      if (!stage.value || !renderer)
        return
      const { width, height } = stage.value.getBoundingClientRect()
      if (!width || !height)
        return
      camera.aspect = width / height
      camera.updateProjectionMatrix()
      renderer.setSize(width, height)
      stageHeight.value = height
    })
    resizeObserver.observe(stage.value)
    frame = requestAnimationFrame(animate)
  } catch (error) {
    console.error('Failed to initialize topology globe', error)
    unavailable.value = true
  }
})

watch([locatedNodes, () => props.links, selectedId], rebuildTopology)
watch(() => props.persistenceKey, restoreView)
watch(rotating, saveView)

onUnmounted(() => {
  saveView()
  disposed = true
  clearTimeout(viewSaveTimer)
  cancelAnimationFrame(frame)
  resizeObserver?.disconnect()
  themeObserver?.disconnect()
  controls?.removeEventListener('change', scheduleZoomSave)
  controls?.dispose()
  renderer?.domElement.removeEventListener('pointerdown', beginInteraction)
  renderer?.domElement.removeEventListener('pointermove', moveInteraction)
  window.removeEventListener('pointerup', endInteraction)
  window.removeEventListener('pointercancel', endInteraction)
  window.removeEventListener('pagehide', saveView)
  renderer?.domElement.removeEventListener('click', pickNode)
  if (scene)
    disposeGroup(scene)
  renderer?.dispose()
})
</script>

<template>
  <section class="topology">
    <header class="topology-header">
      <div>
        <h2>{{ t('web.dashboard.topology') }}</h2>
        <span class="topology-counts">{{ t('web.dashboard.node_count', { count: nodes.length }) }} ·
          {{ t('web.dashboard.link_count', { count: links.length }) }}</span>
      </div>
      <div class="topology-tools">
        <Button :icon="rotating ? 'pi pi-pause' : 'pi pi-play'" text severity="secondary"
          :aria-label="t(rotating ? 'web.dashboard.pause' : 'web.dashboard.rotate')"
          v-tooltip="t(rotating ? 'web.dashboard.pause' : 'web.dashboard.rotate')" @click="rotating = !rotating" />
        <Button icon="pi pi-compass" text severity="secondary" :aria-label="t('web.dashboard.reset')"
          v-tooltip="t('web.dashboard.reset')" @click="resetView()" />
        <Button icon="pi pi-refresh" text severity="secondary" :loading="loading"
          :aria-label="t('web.dashboard.refresh')" v-tooltip="t('web.dashboard.refresh')" @click="emit('refresh')" />
      </div>
    </header>
    <div class="topology-body">
      <div ref="stage" class="globe-stage">
        <div ref="labelLayer" class="globe-labels" />
        <div v-if="unavailable" class="globe-fallback">{{ t('web.dashboard.webgl_unavailable') }}</div>
        <div class="globe-legend">
          <span><i class="managed-dot" />{{ t('web.dashboard.managed') }}</span>
          <span><i class="peer-dot" />{{ t('web.dashboard.peer') }}</span>
        </div>
      </div>
      <aside class="node-panel" :style="{ height: `${stageHeight}px` }">
        <div v-if="!nodes.length" class="node-empty">{{ t('web.dashboard.no_nodes') }}</div>
        <div v-if="unmappedCount" class="unmapped-count">
          {{ t('web.dashboard.unmapped', { count: unmappedCount }) }}
        </div>
        <div class="node-list">
          <button v-for="node in nodes" :key="node.id" type="button" class="node-row"
            :class="{ selected: selectedId === node.id, stale: node.stale }"
            :aria-pressed="selectedId === node.id" @click="selectNode(node.id)">
            <i :class="node.managed ? 'managed-dot' : 'peer-dot'" />
            <span class="node-name" :title="node.label">{{ node.label }}</span>
            <span class="node-country">{{ [node.nodeLocation?.city, node.country].filter(Boolean).join(', ') || t('web.device.unknown_location') }}</span>
          </button>
        </div>
        <div v-if="selectedNode" class="node-detail">
          <strong>{{ selectedNode.label }}</strong>
          <span>Peer ID: {{ selectedNode.peerId }}</span>
          <span v-if="selectedNode.publicIp">{{ selectedNode.publicIp }}</span>
          <span v-if="selectedNode.nodeLocation?.city">{{ [...new Set([selectedNode.country, selectedNode.nodeLocation.region, selectedNode.nodeLocation.city].filter(Boolean))].join(', ') }}</span>
          <span v-if="selectedLocation">≈ {{ selectedLocation.latitude.toFixed(2) }},
            {{ selectedLocation.longitude.toFixed(2) }}</span>
          <span v-else>{{ t('web.device.unknown_location') }}</span>
          <span v-if="selectedNode.stale">{{ t('web.dashboard.retrying') }}</span>
        </div>
      </aside>
    </div>
  </section>
</template>

<style scoped>
.topology {
  --globe-surface: #f1f6f8;
  --globe-land: #087f6c;
  --globe-ocean: #a3bcc6;
  --globe-boundary: #526e76;
  --globe-managed: #b66a09;
  --globe-peer: #ad3974;
  --globe-link: #237aaf;
  --globe-flow-forward: #0b456b;
  --globe-flow-reverse: #0c7169;
  --globe-label-background: rgba(255, 255, 255, 0.94);
  --globe-label-text: #1e3a43;
  --globe-node-text: #8a4d00;
  --globe-stat-text: #375d6b;
  --globe-muted-text: #526b75;
  --globe-label-border: rgba(82, 107, 117, 0.22);
  --globe-leader: rgba(68, 101, 110, 0.46);
  --globe-traffic-leader: rgba(68, 101, 110, 0.82);
  --globe-legend-text: #37515d;
  --globe-scrollbar: #728b91;
  min-width: 0;
}
.topology:where(.app-dark *) {
  --globe-surface: #10191d;
  --globe-land: #67d9b6;
  --globe-ocean: #465f65;
  --globe-boundary: #9bb7b5;
  --globe-managed: #ffcf67;
  --globe-peer: #e99fc4;
  --globe-link: #77bce6;
  --globe-flow-forward: #ffffff;
  --globe-flow-reverse: #daf1ff;
  --globe-label-background: rgba(10, 17, 19, 0.88);
  --globe-label-text: #edf7f5;
  --globe-node-text: #ffda88;
  --globe-stat-text: #c7e2e9;
  --globe-muted-text: #a4bcc0;
  --globe-label-border: rgba(164, 188, 192, 0.18);
  --globe-leader: rgba(187, 215, 213, 0.4);
  --globe-traffic-leader: rgba(187, 215, 213, 0.82);
  --globe-legend-text: #d5e0df;
}
.topology-header { display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-bottom: 12px; }
h2 { margin: 0 0 4px; font-size: 18px; font-weight: 600; }
.topology-counts { font-size: 12px; color: var(--p-text-muted-color); }
.topology-tools { display: flex; flex-shrink: 0; }
.topology-body { display: grid; grid-template-columns: minmax(0, 1fr) 240px; align-items: start; }
.globe-stage { position: relative; width: 100%; aspect-ratio: 1.618 / 1; min-height: 360px; min-width: 0; background: var(--globe-surface); overflow: hidden; }
.globe-stage :deep(canvas) { display: block; width: 100%; height: 100%; touch-action: none; }
.globe-labels { position: absolute; inset: 0; overflow: hidden; pointer-events: none; z-index: 1; }
.globe-labels :deep(.globe-label-stack) { position: absolute; display: flex; flex-direction: column; gap: 4px; box-sizing: border-box; overflow-x: hidden; overflow-y: auto; overscroll-behavior: contain; scrollbar-width: thin; scrollbar-color: var(--globe-scrollbar) transparent; pointer-events: auto; }
.globe-labels :deep(.globe-label-stack::-webkit-scrollbar) { width: 6px; }
.globe-labels :deep(.globe-label-stack::-webkit-scrollbar-thumb) { background: var(--globe-scrollbar); border-radius: 3px; }
.globe-labels :deep(.globe-node-label), .globe-labels :deep(.globe-traffic-label) {
  position: relative; flex-shrink: 0; align-self: flex-start; padding: 3px 5px; max-width: 220px; box-sizing: border-box;
  border-radius: 3px; background: var(--globe-label-background); color: var(--globe-label-text);
  font-size: 11px; line-height: 16px; white-space: pre; overflow: hidden; text-overflow: ellipsis;
}
.globe-labels :deep(.globe-node-label) { width: max-content; max-width: 174px; color: var(--globe-node-text); font-weight: 600; }
.globe-labels :deep(.globe-traffic-label) { width: 198px; max-width: 198px; white-space: normal; }
.globe-labels :deep(.globe-flow-cross) { display: grid; gap: 2px; }
.globe-labels :deep(.traffic-endpoints) { display: grid; grid-template-columns: minmax(0, 1fr) 22px minmax(0, 1fr); gap: 4px; align-items: center; }
.globe-labels :deep(.globe-flow-stat) { min-width: 0; max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; text-align: center; font-size: 10px; line-height: 14px; color: var(--globe-stat-text); }
.globe-labels :deep(.globe-flow-name) { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; pointer-events: auto; }
.globe-labels :deep(.globe-flow-source) { text-align: right; }
.globe-labels :deep(.globe-flow-directions) { text-align: center; font-size: 13px; color: var(--globe-label-text); }
.globe-labels :deep(.globe-flow-latency) { display: flex; justify-content: center; align-items: center; gap: 4px; padding-top: 2px; border-top: 1px solid var(--globe-label-border); color: var(--globe-muted-text); font-size: 10px; line-height: 13px; }
.globe-labels :deep(.globe-flow-latency .pi) { font-size: 10px; }
.globe-labels :deep(.is-stale) { opacity: 0.55; }
.globe-labels :deep(.globe-label-leader) { position: absolute; height: 1px; background: var(--globe-leader); transform-origin: left center; }
.globe-labels :deep(.globe-traffic-leader) { height: 2px; margin-top: -1px; background: var(--globe-traffic-leader); }
.globe-fallback { position: absolute; inset: 0; display: grid; place-items: center; color: var(--globe-muted-text); padding: 24px; text-align: center; }
.globe-legend { position: absolute; bottom: 16px; left: 16px; display: flex; gap: 16px; color: var(--globe-legend-text); font-size: 12px; pointer-events: none; }
.globe-legend span { display: inline-flex; align-items: center; gap: 6px; }
.managed-dot, .peer-dot { display: inline-block; width: 7px; height: 7px; border-radius: 50%; flex-shrink: 0; background: var(--globe-managed); }
.peer-dot { background: var(--globe-peer); }
.node-panel { display: flex; flex-direction: column; min-width: 0; min-height: 0; border-left: 1px solid var(--p-content-border-color); }
.node-list { flex: 1; min-height: 0; overflow: auto; }
.node-row { display: grid; grid-template-columns: 8px minmax(0, 1fr); width: 100%; align-items: center; gap: 4px 8px; border: 0; border-bottom: 1px solid var(--p-content-border-color); padding: 12px; background: transparent; color: inherit; text-align: left; cursor: pointer; font: inherit; }
.node-row:hover, .node-row.selected { background: var(--p-content-hover-background); }
.node-row.stale { opacity: 0.6; }
.node-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 13px; }
.node-country { grid-column: 2; font-size: 11px; color: var(--p-text-muted-color); overflow-wrap: anywhere; }
.node-detail { display: flex; flex-direction: column; gap: 6px; flex-shrink: 0; max-height: 45%; overflow: auto; box-sizing: border-box; border-top: 1px solid var(--p-content-border-color); padding: 12px; font-size: 12px; overflow-wrap: anywhere; }
.node-empty, .unmapped-count { flex-shrink: 0; padding: 12px; font-size: 12px; color: var(--p-text-muted-color); }
@media (max-width: 900px) {
  .topology-body { grid-template-columns: minmax(0, 1fr); }
  .globe-stage { aspect-ratio: 1.4 / 1; min-height: 320px; max-height: none; }
  .node-panel { border-left: 0; border-top: 1px solid var(--p-content-border-color); }
}
@media (max-width: 480px) {
  .globe-stage { aspect-ratio: 1.18 / 1; min-height: 300px; }
  .topology-header { flex-wrap: wrap; }
}
</style>
