<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { Button } from 'primevue'
import { useI18n } from 'vue-i18n'
import { geoEquirectangular, geoPath } from 'd3-geo'
import * as THREE from 'three'
import { OrbitControls } from 'three/addons/controls/OrbitControls.js'
import { locateNode, worldGeography, type LocatedNode } from '../modules/globeGeography'
import type { TopologyLink, TopologyNode } from '../modules/networkTopology'

const props = defineProps<{
  nodes: TopologyNode[]
  links: TopologyLink[]
  loading?: boolean
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
let topologyGroup: THREE.Group | undefined
let globeSurface: THREE.Mesh | undefined
let boundaryGroup: THREE.Group | undefined
let boundaryMaterial: THREE.LineBasicMaterial | undefined
let cloudLevels: { group: THREE.Group, maxDistance: number }[] = []
let activeCloudLevel = -1
let frame = 0
let lastFrame = 0
let interacting = false
let interactionStart: { x: number, y: number } | undefined
let dragged = false
let markerMeshes: THREE.Mesh[] = []
let flows: { mesh: THREE.Mesh, curve: THREE.CatmullRomCurve3, offset: number, reverse: boolean }[] = []
interface GlobeLabel {
  element: HTMLDivElement
  leader: HTMLDivElement
  anchors: THREE.Vector3[]
  priority: number
}
let labels: GlobeLabel[] = []
const raycaster = new THREE.Raycaster()
const pointer = new THREE.Vector2()
const markerViewPosition = new THREE.Vector3()
const labelProjection = new THREE.Vector3()
const labelDirection = new THREE.Vector3()
const labelIntersection = new THREE.Vector3()
const labelRay = new THREE.Ray()
const globeOccluder = new THREE.Sphere(new THREE.Vector3(), 0.994)
const controlsConfig = {
  rotateSpeed: 0.26,
  zoomSpeed: 0.78,
  minDistance: 1.25,
  maxDistance: 6.5,
  autoRotateSpeed: 0.21,
}

function position(latitude: number, longitude: number, radius = 1) {
  const lat = THREE.MathUtils.degToRad(latitude)
  const lon = THREE.MathUtils.degToRad(longitude)
  return new THREE.Vector3(
    radius * Math.cos(lat) * Math.sin(lon),
    radius * Math.sin(lat),
    radius * Math.cos(lat) * Math.cos(lon),
  )
}

function disposeGroup(group: THREE.Object3D) {
  group.traverse(object => {
    const renderable = object as THREE.Mesh
    renderable.geometry?.dispose()
    if (Array.isArray(renderable.material))
      renderable.material.forEach(material => material.dispose())
    else
      renderable.material?.dispose()
  })
}

function sphericalArc(start: THREE.Vector3, end: THREE.Vector3, radius: number) {
  const from = start.clone().normalize()
  const to = end.clone().normalize()
  const angle = from.angleTo(to)
  if (angle < 0.0001)
    return [from.multiplyScalar(radius)]
  let axis = new THREE.Vector3().crossVectors(from, to)
  if (axis.lengthSq() < 0.00001)
    axis = new THREE.Vector3(0, 1, 0)
  else
    axis.normalize()
  const segments = Math.max(1, Math.ceil(angle / 0.045))
  return Array.from({ length: segments + 1 }, (_, index) =>
    from.clone().applyAxisAngle(axis, angle * index / segments).multiplyScalar(radius))
}

function buildCountryBoundaries() {
  const features = (worldGeography as unknown as {
    features?: { geometry?: { type?: string, coordinates?: unknown } }[]
  }).features ?? []
  boundaryGroup = new THREE.Group()
  boundaryMaterial = new THREE.LineBasicMaterial({
    color: 0x9bb7b5,
    transparent: true,
    opacity: 0.38,
    depthWrite: false,
  })
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
        const points: number[] = []
        for (let index = 0; index < coordinates.length - 1; index++) {
          const [sourceLongitude, sourceLatitude] = coordinates[index]
          const [targetLongitude, targetLatitude] = coordinates[index + 1]
          const arc = sphericalArc(
            position(sourceLatitude, sourceLongitude),
            position(targetLatitude, targetLongitude),
            1.003,
          )
          for (const point of index === 0 ? arc : arc.slice(1))
            points.push(point.x, point.y, point.z)
        }
        if (points.length < 6)
          continue
        const lineGeometry = new THREE.BufferGeometry()
        lineGeometry.setAttribute('position', new THREE.Float32BufferAttribute(points, 3))
        boundaryGroup.add(new THREE.Line(lineGeometry, boundaryMaterial))
      }
    }
  }
  scene.add(boundaryGroup)
}

function buildPointCloud(
  map: HTMLCanvasElement,
  pixels: Uint8ClampedArray,
  count: number,
  oceanStride: number,
) {
  const land: number[] = []
  const ocean: number[] = []
  const goldenAngle = Math.PI * (3 - Math.sqrt(5))
  for (let i = 0; i < count; i++) {
    const lat = Math.asin(1 - 2 * (i + 0.5) / count) * 180 / Math.PI
    const lon = ((i * goldenAngle * 180 / Math.PI) % 360) - 180
    const x = Math.min(map.width - 1, Math.floor((lon + 180) / 360 * map.width))
    const y = Math.min(map.height - 1, Math.floor((90 - lat) / 180 * map.height))
    const isLand = pixels[(y * map.width + x) * 4 + 3] > 100
    if (!isLand && i % oceanStride !== 0)
      continue
    const point = position(lat, lon)
    ;(isLand ? land : ocean).push(point.x, point.y, point.z)
  }
  const group = new THREE.Group()
  for (const [points, color, size] of [
    [land, 0x67d9b6, count > 100_000 ? 0.0035 : count > 30_000 ? 0.006 : 0.011],
    [ocean, 0x465f65, count > 100_000 ? 0.002 : count > 30_000 ? 0.0035 : 0.006],
  ] as const) {
    const geometry = new THREE.BufferGeometry()
    geometry.setAttribute('position', new THREE.Float32BufferAttribute(points, 3))
    group.add(new THREE.Points(geometry, new THREE.PointsMaterial({
      color,
      size,
      sizeAttenuation: true,
    })))
  }
  return group
}

function buildCloud() {
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
  geoPath(projection, context)(worldGeography)
  context.fill()
  const pixels = context.getImageData(0, 0, map.width, map.height).data
  const lowDetail = buildPointCloud(map, pixels, 24_000, 4)
  const highDetail = buildPointCloud(map, pixels, 96_000, 3)
  const closeDetail = buildPointCloud(map, pixels, 288_000, 3)
  lowDetail.visible = true
  highDetail.visible = false
  closeDetail.visible = false
  cloudLevels = [
    { group: lowDetail, maxDistance: Number.POSITIVE_INFINITY },
    { group: highDetail, maxDistance: 2.45 },
    { group: closeDetail, maxDistance: 1.55 },
  ]
  activeCloudLevel = 0
  scene.add(lowDetail, highDetail, closeDetail)
  globeSurface = new THREE.Mesh(
    new THREE.SphereGeometry(0.994, 64, 32),
    new THREE.MeshBasicMaterial({ color: 0x10191d }),
  )
  scene.add(globeSurface)
  buildCountryBoundaries()
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
  const nextLevel = distance <= cloudLevels[2]?.maxDistance ? 2
    : distance <= cloudLevels[1]?.maxDistance ? 1 : 0
  if (nextLevel !== activeCloudLevel) {
    cloudLevels.forEach(({ group }, index) => {
      group.visible = index === nextLevel
    })
    activeCloudLevel = nextLevel
  }
  if (renderer) {
    renderer.domElement.dataset.globeRotateSpeed = String(controls.rotateSpeed)
    renderer.domElement.dataset.globeDistance = String(distance)
    renderer.domElement.dataset.globeCloudLevel = String(nextLevel)
    renderer.domElement.dataset.globeAzimuth = String(controls.getAzimuthalAngle())
  }
  if (boundaryMaterial) {
    const zoom = THREE.MathUtils.clamp((5 - distance) / 3.75, 0, 1)
    boundaryMaterial.opacity = THREE.MathUtils.lerp(0.35, 0.72, zoom)
  }
}

function scaleMarker(mesh: THREE.Mesh, radius: number, pixels: number) {
  if (!renderer)
    return
  markerViewPosition.copy(mesh.position).applyMatrix4(camera.matrixWorldInverse)
  const unitsPerPixel = -markerViewPosition.z
    * 2 * Math.tan(THREE.MathUtils.degToRad(camera.fov / 2))
    / Math.max(1, renderer.domElement.clientHeight)
  mesh.scale.setScalar(Math.max(0.001, unitsPerPixel * pixels / radius))
}

function formatLiveRate(value: number | undefined) {
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

function flowDensity(rate: number | undefined, stale?: boolean) {
  if (stale || rate === undefined || !Number.isFinite(rate) || rate <= 0)
    return 0
  return THREE.MathUtils.clamp(1 + Math.floor(Math.log10(Math.max(1, rate / 1000)) * 2), 1, 16)
}

function addLabel(text: string, kind: 'node' | 'traffic', anchors: THREE.Vector3[], priority: number, stale?: boolean) {
  if (!labelLayer.value)
    return
  const element = document.createElement('div')
  element.className = `globe-${kind}-label${stale ? ' is-stale' : ''}`
  element.textContent = text
  element.style.display = 'none'
  const leader = document.createElement('div')
  leader.className = 'globe-label-leader'
  leader.style.display = 'none'
  labelLayer.value.append(leader, element)
  labels.push({ element, leader, anchors, priority })
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
  const close = camera.position.distanceTo(controls.target) <= 2.45
  const { clientWidth: width, clientHeight: height } = renderer.domElement
  labelLayer.value.dataset.detailVisible = String(close)
  const occupied: { x: number, y: number, width: number, height: number }[] = []
  for (const label of labels) {
    label.element.style.display = 'none'
    label.leader.style.display = 'none'
  }
  if (!close)
    return
  for (const label of [...labels].sort((left, right) => right.priority - left.priority)) {
    const anchor = label.anchors.map(point => projectedAnchor(point, width, height)).find(Boolean)
    if (!anchor)
      continue
    label.element.style.display = 'block'
    const labelWidth = label.element.offsetWidth
    const labelHeight = label.element.offsetHeight
    let placed = false
    for (const column of [1, -1]) {
      for (const row of [0, -1, 1, -2, 2, -3, 3, -4, 4]) {
        const x = THREE.MathUtils.clamp(
          anchor.x + (column > 0 ? 10 : -labelWidth - 10), 5, Math.max(5, width - labelWidth - 5),
        )
        const y = THREE.MathUtils.clamp(
          anchor.y - labelHeight / 2 + row * (labelHeight + 5), 5, Math.max(5, height - labelHeight - 34),
        )
        if (occupied.some(rect => x < rect.x + rect.width + 4 && x + labelWidth + 4 > rect.x
          && y < rect.y + rect.height + 4 && y + labelHeight + 4 > rect.y))
          continue
        occupied.push({ x, y, width: labelWidth, height: labelHeight })
        label.element.style.left = `${x}px`
        label.element.style.top = `${y}px`
        const endX = THREE.MathUtils.clamp(anchor.x, x, x + labelWidth)
        const endY = THREE.MathUtils.clamp(anchor.y, y, y + labelHeight)
        const dx = endX - anchor.x
        const dy = endY - anchor.y
        label.leader.style.display = 'block'
        label.leader.style.left = `${anchor.x}px`
        label.leader.style.top = `${anchor.y}px`
        label.leader.style.width = `${Math.hypot(dx, dy)}px`
        label.leader.style.transform = `rotate(${Math.atan2(dy, dx)}rad)`
        placed = true
        break
      }
      if (placed)
        break
    }
    if (!placed)
      label.element.style.display = 'none'
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
  interacting = false
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
  if (topologyGroup) {
    scene.remove(topologyGroup)
    disposeGroup(topologyGroup)
  }
  topologyGroup = new THREE.Group()
  markerMeshes = []
  flows = []
  labels = []
  labelLayer.value?.replaceChildren()
  const nodeMap = new Map<string, LocatedNode>(locatedNodes.value.map(node => [node.id, node]))
  for (const node of nodeMap.values()) {
    const mesh = new THREE.Mesh(
      new THREE.SphereGeometry(node.id === selectedId.value ? 0.025 : 0.018, 12, 8),
      new THREE.MeshBasicMaterial({
        color: node.managed ? 0xffcf67 : 0xe99fc4,
        transparent: !!node.stale,
        opacity: node.stale ? 0.5 : 1,
      }),
    )
    mesh.position.copy(position(node.latitude, node.longitude, 1.018))
    mesh.userData.nodeId = node.id
    mesh.userData.radius = node.id === selectedId.value ? 0.025 : 0.018
    markerMeshes.push(mesh)
    topologyGroup.add(mesh)
    addLabel(node.label, 'node', [mesh.position.clone()], node.id === selectedId.value ? 10 : 6, node.stale)
  }
  for (const [index, link] of props.links.entries()) {
    const source = nodeMap.get(link.source)
    const target = nodeMap.get(link.target)
    if (!source || !target)
      continue
    const curve = linkCurve(position(source.latitude, source.longitude), position(target.latitude, target.longitude))
    topologyGroup.add(new THREE.Line(
      new THREE.BufferGeometry().setFromPoints(curve.getPoints(64)),
      new THREE.LineBasicMaterial({ color: 0x77bce6, transparent: true, opacity: link.stale ? 0.28 : 0.72 }),
    ))
    for (const [reverse, rate] of [[false, link.txBps], [true, link.rxBps]] as const) {
      const count = flowDensity(rate, link.stale)
      for (let particle = 0; particle < count; particle++) {
        const mesh = new THREE.Mesh(
          new THREE.SphereGeometry(0.009, 8, 6),
          new THREE.MeshBasicMaterial({ color: reverse ? 0xdaf1ff : 0xffffff }),
        )
        flows.push({ mesh, curve, offset: index * 0.23 + particle / count + (reverse ? 0.37 : 0), reverse })
        topologyGroup.add(mesh)
      }
    }
    const shortName = (name: string) => [...name].length > 6
      ? `${[...name].slice(0, 2).join('')}...${[...name].slice(-2).join('')}` : name
    const from = shortName(source.label)
    const to = shortName(target.label)
    addLabel(
      `${from} → ${to}  ${formatLiveRate(link.txBps)}\n${to} → ${from}  ${formatLiveRate(link.rxBps)}`,
      'traffic',
      [0.5, 0.35, 0.65, 0.2, 0.8, 0.1, 0.9, 0.05, 0.95, 0.02, 0.98].map(progress => curve.getPoint(progress)),
      link.source === selectedId.value || link.target === selectedId.value ? 5 : 1,
      link.stale,
    )
  }
  scene.add(topologyGroup)
  renderer.domElement.dataset.globeForwardParticles = String(flows.filter(flow => !flow.reverse).length)
  renderer.domElement.dataset.globeReverseParticles = String(flows.filter(flow => flow.reverse).length)
}

function selectNode(id: string) {
  selectedId.value = id
  const node = locatedNodes.value.find(node => node.id === id)
  if (!node || !camera || !controls)
    return
  const distance = camera.position.distanceTo(controls.target)
  camera.position.copy(position(node.latitude, node.longitude, distance))
  controls.update()
}

function resetView() {
  camera?.position.copy(position(20, 100, 3.3))
  controls?.target.set(0, 0, 0)
  controls?.update()
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
    controls.update(deltaSeconds)
    updateCloudDetail()
    for (const mesh of markerMeshes)
      scaleMarker(mesh, mesh.userData.radius, mesh.userData.nodeId === selectedId.value ? 7 : 5)
    for (const flow of flows) {
      const progress = (now / 5000 + flow.offset) % 1
      flow.mesh.position.copy(flow.curve.getPointAt(flow.reverse ? 1 - progress : progress))
      scaleMarker(flow.mesh, 0.009, 2)
    }
    renderer.domElement.dataset.globeInvalidFlows = String(
      flows.filter(flow => ![flow.mesh.position.x, flow.mesh.position.y, flow.mesh.position.z]
        .every(Number.isFinite)).length,
    )
    renderer.render(scene, camera)
    updateLabels()
    lastFrame = now
  }
  frame = requestAnimationFrame(animate)
}

onMounted(() => {
  if (!stage.value)
    return
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
    resetView()
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

onUnmounted(() => {
  cancelAnimationFrame(frame)
  resizeObserver?.disconnect()
  controls?.dispose()
  renderer?.domElement.removeEventListener('pointerdown', beginInteraction)
  renderer?.domElement.removeEventListener('pointermove', moveInteraction)
  window.removeEventListener('pointerup', endInteraction)
  window.removeEventListener('pointercancel', endInteraction)
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
          v-tooltip="t('web.dashboard.reset')" @click="resetView" />
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
            :class="{ selected: selectedId === node.id, stale: node.stale }" @click="selectNode(node.id)">
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
.topology { min-width: 0; }
.topology-header { display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-bottom: 12px; }
h2 { margin: 0 0 4px; font-size: 18px; font-weight: 600; }
.topology-counts { font-size: 12px; color: var(--p-text-muted-color); }
.topology-tools { display: flex; flex-shrink: 0; }
.topology-body { display: grid; grid-template-columns: minmax(0, 1fr) 240px; align-items: start; }
.globe-stage { position: relative; width: 100%; aspect-ratio: 1.618 / 1; min-height: 360px; min-width: 0; background: #10191d; overflow: hidden; }
.globe-stage :deep(canvas) { display: block; width: 100%; height: 100%; touch-action: none; }
.globe-labels { position: absolute; inset: 0; overflow: hidden; pointer-events: none; z-index: 1; }
.globe-labels :deep(.globe-node-label), .globe-labels :deep(.globe-traffic-label) {
  position: absolute; padding: 3px 5px; max-width: 220px; box-sizing: border-box;
  border-radius: 3px; background: rgba(10, 17, 19, 0.88); color: #edf7f5;
  font-size: 11px; line-height: 16px; white-space: pre; overflow: hidden; text-overflow: ellipsis;
}
.globe-labels :deep(.globe-node-label) { max-width: 174px; color: #ffda88; font-weight: 600; }
.globe-labels :deep(.is-stale) { opacity: 0.55; }
.globe-labels :deep(.globe-label-leader) { position: absolute; height: 1px; background: rgba(187, 215, 213, 0.4); transform-origin: left center; }
.globe-fallback { position: absolute; inset: 0; display: grid; place-items: center; color: #c6d9db; padding: 24px; text-align: center; }
.globe-legend { position: absolute; bottom: 16px; left: 16px; display: flex; gap: 16px; color: #d5e0df; font-size: 12px; pointer-events: none; }
.globe-legend span { display: inline-flex; align-items: center; gap: 6px; }
.managed-dot, .peer-dot { display: inline-block; width: 7px; height: 7px; border-radius: 50%; flex-shrink: 0; background: #ffcf67; }
.peer-dot { background: #e99fc4; }
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
