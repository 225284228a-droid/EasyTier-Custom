import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
import { mkdir } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const require = createRequire(import.meta.url)
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright')
const { PNG } = require(process.env.PNGJS_MODULE || 'pngjs')
const baseUrl = process.env.DASHBOARD_URL || 'http://127.0.0.1:5187'
const output = process.env.DASHBOARD_QA_OUTPUT || join(tmpdir(), 'easytier-dashboard-qa')
await mkdir(output, { recursive: true })
const uuid = (id) => ({ part1: 0, part2: 0, part3: 0, part4: id })
const uuidString = (id) => `00000000-0000-0000-0000-${id.toString(16).padStart(12, '0')}`
const locations = [
  { country: 'China', latitude: 31.2, longitude: 121.5 },
  { country: 'New Zealand', latitude: -36.8, longitude: 174.7 },
  { country: 'Singapore', latitude: 1.3, longitude: 103.8 },
  { country: 'Unknown' },
]
const trafficStarted = Date.now()
let jitter = false
let colocated = false
let hangNext = false
let hungRoute
let trafficScale = 1
let collectionDelay = 0
const machineItems = locations.map((location, index) => ({
  client_url: `tcp://198.51.100.${index + 1}:11010`,
  location,
  info: {
    machine_id: uuid(index + 1),
    hostname: ['Shanghai', 'Auckland', 'Singapore', 'Unresolved'][index],
    running_network_instances: [uuid(index + 11)],
  },
}))
const snapshot = (index) => ({
  running: true,
  network_name: 'qa-mesh',
  my_node_info: { peer_id: index + 1 },
  node_location: {
    public_ip: `203.0.113.${index + 1}`,
    country: colocated ? 'China' : locations[index].country,
    city: colocated ? 'Shanghai' : ['Shanghai', 'Auckland', 'Singapore', ''][index],
    region: '',
    latitude: colocated ? 31.2 : locations[index].latitude,
    longitude: colocated ? 121.5 : locations[index].longitude,
  },
  routes: [0, 1, 2].filter(peer => peer !== index).map(peer => ({
    peer_id: peer + 1, inst_id: uuidString(peer + 11), cost: 1,
  })),
  peers: [0, 1, 2].filter(peer => peer !== index).map(peer => ({
    peer_id: peer + 1,
    conns: [{
      conn_id: uuidString(peer + 101), is_closed: false, tunnel: { tunnel_type: 'udp' },
      stats: {
        latency_us: String(index === 0 || peer === 0
          ? (index === 1 || peer === 1 ? 5_000 : 50_000) : 150_000),
        tx_bytes: String(Math.floor((Date.now() - trafficStarted) / 1000
          * (index < peer ? 2_500_000 : 25_000) * trafficScale)),
        rx_bytes: String(Math.floor((Date.now() - trafficStarted) / 1000
          * (index < peer ? 25_000 : 2_500_000) * trafficScale)),
      },
    }],
  })),
})

async function assertLocalLabelLayout(page, name) {
  const layout = await page.locator('.globe-labels').evaluate(element => {
    const stage = element.getBoundingClientRect()
    return [...element.querySelectorAll('.globe-label-stack')]
      .filter(stack => getComputedStyle(stack).display !== 'none')
      .map(stack => {
        const rect = stack.getBoundingClientRect()
        return {
          id: stack.getAttribute('data-stack-id'),
          traffic: stack.classList.contains('globe-traffic-stack'),
          x: rect.x - stage.x, y: rect.y - stage.y,
          width: rect.width, height: rect.height,
          stageWidth: stage.width, stageHeight: stage.height,
          leaderLength: Number.parseFloat(stack.previousElementSibling.style.width),
          leaderThickness: Number.parseFloat(getComputedStyle(stack.previousElementSibling).height),
          rows: [...stack.children].map(label => {
            const row = label.getBoundingClientRect()
            return { id: label.getAttribute('data-label-id'), x: row.x, y: row.y, height: row.height }
          }),
        }
      })
  })
  assert.equal(new Set(layout.map(label => label.id)).size, layout.length, `${name}: duplicate link labels`)
  const ids = layout.flatMap(stack => stack.rows.map(row => row.id))
  assert.equal(new Set(ids).size, ids.length, `${name}: grouped labels merged or duplicated device links`)
  layout.forEach((label, index) => {
    label.rows.forEach((row, rowIndex) => {
      assert.ok(Math.abs(row.x - label.rows[0].x) < 0.1, `${name}: grouped labels are not vertically aligned`)
      if (rowIndex) {
        const previous = label.rows[rowIndex - 1]
        assert.ok(Math.abs(row.y - previous.y - previous.height - 4) < 0.1,
          `${name}: grouped rows do not keep their vertical gap`)
      }
    })
    assert.ok(label.leaderLength <= (label.traffic ? 48 : 32) + 0.1,
      `${name}: a label connector stretched away from its link`)
    if (label.traffic)
      assert.ok(label.leaderThickness >= 2, `${name}: traffic connector is too thin`)
    assert.ok(label.x >= 4.9 && label.y >= 4.9
      && label.x + label.width <= label.stageWidth - 4.9
      && label.y + label.height <= label.stageHeight - 33.9, `${name}: label exceeds stage bounds`)
    for (const previous of layout.slice(0, index)) {
      assert.equal(label.x < previous.x + previous.width + 3.5
        && label.x + label.width + 3.5 > previous.x
        && label.y < previous.y + previous.height + 3.5
        && label.y + label.height + 3.5 > previous.y, false, `${name}: labels overlap`)
    }
  })
  return layout
}

const browser = await chromium.launch({
  executablePath: process.env.CHROMIUM_EXECUTABLE || undefined,
  headless: true,
  args: ['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'],
})
const errors = []
let page
try {
  page = await browser.newPage()
  page.on('pageerror', error => errors.push(error.message))
  let empty = false
  await page.route('**/api/v1/**', async route => {
    const pathname = new URL(route.request().url()).pathname
    let body = {}
    if (pathname.endsWith('/machines')) {
      body = { machines: empty ? [] : machineItems }
    } else if (pathname.endsWith('/networks/info')) {
      assert.equal(route.request().method(), 'POST')
      const id = Number.parseInt(pathname.match(/machines\/([^/]+)/)[1].replaceAll('-', ''), 16)
      if (hangNext && id === 2) {
        hangNext = false
        hungRoute = route
        return
      }
      if (jitter && id === 2) {
        await route.fulfill({ status: 503, contentType: 'application/json', body: '{"message":"temporary"}' })
        return
      }
      if (collectionDelay)
        await new Promise(resolve => setTimeout(resolve, collectionDelay))
      body = { info: { map: { [uuidString(id + 10)]: snapshot(id - 1) } } }
    }
    await route.fulfill({ contentType: 'application/json', body: JSON.stringify(body) })
  })
  await page.addInitScript(() => {
    if (location.protocol === 'http:' || location.protocol === 'https:')
      localStorage.setItem('lang', 'en')
  })
  const dashboardUrl = `${baseUrl}/#/h/${btoa(baseUrl)}`
  for (const [name, viewport] of [
    ['desktop', { width: 1440, height: 1000 }],
    ['mobile', { width: 390, height: 844 }],
  ]) {
    await page.goto('about:blank')
    await page.context().clearCookies()
    await page.setViewportSize(viewport)
    await page.goto(dashboardUrl)
    await page.reload()
    const canvas = page.locator('.globe-stage canvas')
    await canvas.waitFor()
    await page.locator('.node-row').first().waitFor()
    const defaultLongitude = 114.17 * Math.PI / 180
    await page.waitForFunction(() => document.querySelector('.globe-stage canvas')?.hasAttribute('data-globe-azimuth'))
    const initialAzimuth = Number(await canvas.getAttribute('data-globe-azimuth'))
    assert.ok(Math.abs(Math.atan2(Math.sin(initialAzimuth - defaultLongitude),
      Math.cos(initialAzimuth - defaultLongitude))) < 0.08,
    `${name}: default globe view is not centered near Hong Kong`)
    const summaries = await page.locator('.dashboard-summary strong').allTextContents()
    assert.deepEqual(summaries, ['4', '1', '6'], `${name}: machines sharing a mesh must count as one network`)
    const globeControls = await canvas.evaluate((element) => ({
      rotateSpeed: Number(element.dataset.globeRotateSpeed),
      zoomSpeed: Number(element.dataset.globeZoomSpeed),
      minDistance: Number(element.dataset.globeMinDistance),
      maxDistance: Number(element.dataset.globeMaxDistance),
      cloudLevels: element.dataset.globeCloudLevels,
      aspectRatio: getComputedStyle(element.parentElement).aspectRatio,
    }))
    assert.ok(globeControls.rotateSpeed > 0 && globeControls.rotateSpeed < 0.6,
      `${name}: drag rotation should use a reduced, predictable sensitivity`)
    assert.ok(globeControls.zoomSpeed > 0 && globeControls.zoomSpeed < 1,
      `${name}: wheel/pinch zoom should not use an aggressive default speed`)
    assert.ok(globeControls.minDistance < 1.7 && globeControls.maxDistance > 5,
      `${name}: globe zoom range should expose a closer high-detail view`)
    assert.equal(globeControls.cloudLevels, '24000,96000,288000',
      `${name}: globe should advertise adaptive point cloud levels`)
    const expectedAspect = name === 'desktop' ? '1.618' : '1.18'
    assert.match(globeControls.aspectRatio, new RegExp(expectedAspect),
      `${name}: globe stage should keep its responsive aspect ratio`)
    await page.waitForTimeout(800)
    const first = await canvas.screenshot()
    const image = PNG.sync.read(first)
    let landPixels = 0
    for (let i = 0; i < image.data.length; i += 4) {
      const [r, g, b] = image.data.subarray(i, i + 3)
      if (g > 90 && g > r * 1.25 && b > 60)
        landPixels++
    }
    assert.ok(landPixels > 400, `${name}: globe has no visible continent point cloud`)
    await page.waitForTimeout(1200)
    assert.notDeepEqual(await canvas.screenshot(), first, `${name}: animation is static`)
    await page.waitForFunction(() => {
      const canvas = document.querySelector('.globe-stage canvas')
      return Number(canvas?.getAttribute('data-globe-forward-particles')) > 0
        && Number(canvas?.getAttribute('data-globe-reverse-particles')) > 0
    })
    assert.ok(Number(await canvas.getAttribute('data-globe-forward-emission-rate'))
      > Number(await canvas.getAttribute('data-globe-reverse-emission-rate')) * 4,
      `${name}: emission frequency must distinguish different directional traffic`)
    const timings = JSON.parse(await canvas.getAttribute('data-globe-flow-timings'))
      .filter(flow => JSON.parse(flow.key)[2] === false)
      .sort((left, right) => left.travelSeconds - right.travelSeconds)
    assert.equal(timings.length, 3)
    assert.ok(timings.every(flow => flow.emissionsPerSecond < 6),
      `${name}: busy links immediately saturated the emission density`)
    assert.ok(Math.abs(timings[0].emissionsPerSecond - timings[2].emissionsPerSecond) < 0.1,
      `${name}: equal throughput must emit equally despite different RTTs`)
    assert.ok(Math.abs(timings[1].travelSeconds / timings[0].travelSeconds - 10) < 0.01,
      `${name}: low-latency travel-time differences were compressed`)
    await page.locator('.node-row').filter({ hasText: 'Auckland' }).click()
    const lowBoundaryVertices = Number(await canvas.getAttribute('data-globe-boundary-source-vertices'))
    await assert.doesNotReject(() => page.locator('.node-detail').waitFor())
    assert.match(await page.locator('.node-detail').innerText(), /-36\.80/)
    assert.ok(await page.locator('.node-list').evaluate(element => element.clientHeight) > 40,
      `${name}: node details collapsed the node list`)
    const panelHeight = await page.locator('.node-panel').evaluate(element => element.clientHeight)
    const mapHeight = await page.locator('.globe-stage').evaluate(element => element.clientHeight)
    assert.ok(Math.abs(panelHeight - mapHeight) <= 2,
      `${name}: node panel must match the map height`)
    if (name === 'mobile') {
      assert.ok(await page.locator('.node-list').evaluate(element => element.scrollHeight > element.clientHeight),
        'mobile: long node lists must scroll inside the fixed-height panel')
    }
    assert.equal(await page.locator('footer.geoip-attribution').count(), 0,
      `${name}: geolocation source notice should not remain on the dashboard`)
    await page.getByRole('button', { name: 'Pause Rotation', exact: true }).click()
    const box = await canvas.boundingBox()
    const azimuthBefore = await canvas.getAttribute('data-globe-azimuth')
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2)
    await page.mouse.down()
    await page.mouse.move(box.x + box.width / 2 + 24, box.y + box.height / 2, { steps: 5 })
    await page.mouse.up()
    await page.waitForTimeout(100)
    const azimuthAfter = await canvas.getAttribute('data-globe-azimuth')
    const angularChange = Math.abs(Math.atan2(
      Math.sin(Number(azimuthAfter) - Number(azimuthBefore)),
      Math.cos(Number(azimuthAfter) - Number(azimuthBefore)),
    ))
    assert.ok(angularChange > 0.015 && angularChange < 0.18, `${name}: a small drag caused excessive rotation`)
    await page.waitForTimeout(300)
    assert.equal(await canvas.getAttribute('data-globe-azimuth'), azimuthAfter,
      `${name}: globe continued spinning after a manual drag`)
    const motionBefore = PNG.sync.read(await canvas.screenshot())
    await page.waitForTimeout(200)
    const motionAfter = PNG.sync.read(await canvas.screenshot())
    let movingWhitePixels = 0
    for (let i = 0; i < motionBefore.data.length; i += 4) {
      const before = motionBefore.data[i] > 230 && motionBefore.data[i + 1] > 230 && motionBefore.data[i + 2] > 230
      const after = motionAfter.data[i] > 230 && motionAfter.data[i + 1] > 230 && motionAfter.data[i + 2] > 230
      if (before !== after)
        movingWhitePixels++
    }
    assert.ok(movingWhitePixels > 2, `${name}: particle pixels did not move on the paused globe`)
    const beforeCancellation = await canvas.getAttribute('data-globe-azimuth')
    await page.locator('.node-row').filter({ hasText: 'Auckland' }).click()
    assert.equal(await page.locator('.node-row.selected').count(), 0,
      `${name}: clicking the selected node did not cancel selection`)
    assert.equal(await page.locator('.node-detail').count(), 0,
      `${name}: canceled selection left the detail panel visible`)
    assert.equal(await page.locator('.node-row').filter({ hasText: 'Auckland' }).getAttribute('aria-pressed'), 'false')
    assert.equal(await canvas.getAttribute('data-globe-azimuth'), beforeCancellation,
      `${name}: canceling a node unexpectedly moved the camera`)
    await page.locator('.node-row').filter({ hasText: 'Auckland' }).click()
    assert.equal(await page.locator('.node-row').filter({ hasText: 'Auckland' }).getAttribute('aria-pressed'), 'true')
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2)
    for (let i = 0; i < 15 && Number(await canvas.getAttribute('data-globe-distance')) > 2.75; i++) {
      await page.mouse.wheel(0, -150)
      await page.waitForTimeout(50)
    }
    assert.ok(Number(await canvas.getAttribute('data-globe-distance')) > 1.7,
      `${name}: intermediate zoom unexpectedly skipped the name-only phase`)
    assert.ok(await page.locator('.globe-node-label').evaluateAll(elements =>
      elements.some(element => getComputedStyle(element).display !== 'none')),
    `${name}: node names must become visible before traffic labels`)
    assert.ok(await page.locator('.globe-traffic-label').evaluateAll(elements =>
      elements.every(element => getComputedStyle(element).display === 'none')),
    `${name}: traffic labels became visible too early`)
    await page.mouse.wheel(0, -2500)
    await page.waitForFunction(() =>
      document.querySelector('.globe-stage canvas')?.getAttribute('data-globe-cloud-level') === '2',
    undefined, { timeout: 30_000 })
    assert.equal(await canvas.getAttribute('data-globe-cloud-level'), '2',
      `${name}: close zoom did not select the dense point cloud`)
    assert.equal(await canvas.getAttribute('data-globe-boundary-scale'), '10m',
      `${name}: close zoom did not select actual high-resolution boundary data`)
    assert.ok(Number(await canvas.getAttribute('data-globe-boundary-source-vertices')) > lowBoundaryVertices * 5,
      `${name}: zoom detail only subdivided old low-resolution boundaries`)
    const stageBounds = await page.locator('.globe-stage').boundingBox()
    assert.ok(stageBounds.x + stageBounds.width <= viewport.width + 1,
      `${name}: globe stage exceeds the viewport`)
    await page.waitForFunction(() => [...document.querySelectorAll('.globe-node-label')]
      .some(element => getComputedStyle(element).display !== 'none'))
    await page.waitForFunction(() => [...document.querySelectorAll('.globe-traffic-label')]
      .some(element => getComputedStyle(element).display !== 'none' && element.textContent.includes('bit/s')))
    assert.ok(await page.locator('.globe-traffic-label').evaluateAll(elements =>
      elements.some(element => getComputedStyle(element).display !== 'none'
        && element.querySelector('.traffic-endpoints')
        && element.querySelector('.globe-flow-latency-value')?.textContent.match(/^RTT [\d.]+ ms$/)
        && [...element.querySelectorAll('.globe-flow-stat')].every(stat => !stat.textContent.includes('RTT')))),
    `${name}: traffic labels must keep directional rates separate from the RTT footer`)
    await assertLocalLabelLayout(page, name)
    const continuity = await page.locator('.globe-labels').evaluateHandle(layer => {
      const tracked = [...layer.querySelectorAll('.globe-traffic-label')]
        .filter(element => getComputedStyle(element).display !== 'none')
        .map(element => ({
          element, stack: element.closest('.globe-label-stack'),
          leader: element.closest('.globe-label-stack').previousElementSibling,
          rate: element.querySelector('.globe-flow-source-stat'),
        }))
      const state = { frames: 0, hiddenFrames: 0, replacedFrames: 0, frame: 0 }
      const sample = () => {
        state.frames++
        if (tracked.some(({ element, stack, leader, rate }) => !element.isConnected
          || element.parentElement !== stack || stack.previousElementSibling !== leader
          || element.querySelector('.globe-flow-source-stat') !== rate))
          state.replacedFrames++
        if (tracked.some(({ element, stack }) => getComputedStyle(element).display === 'none'
          || getComputedStyle(stack).display === 'none'))
          state.hiddenFrames++
        state.frame = requestAnimationFrame(sample)
      }
      state.frame = requestAnimationFrame(sample)
      return state
    })
    collectionDelay = 400
    const refreshButton = page.getByRole('button', { name: 'Refresh Topology', exact: true })
    const automaticResponse = page.waitForResponse(response =>
      response.url().includes('/networks/info') && response.status() === 200)
    await page.waitForRequest(request => request.url().includes('/networks/info'))
    assert.equal(await refreshButton.locator('[data-pc-section="loadingicon"]').count(), 0,
      `${name}: automatic polling activated the manual refresh spinner`)
    await automaticResponse
    await page.waitForTimeout(700)
    const manualResponse = page.waitForResponse(response =>
      response.url().includes('/networks/info') && response.status() === 200)
    await refreshButton.click()
    await refreshButton.locator('[data-pc-section="loadingicon"]').waitFor()
    await manualResponse
    await page.waitForTimeout(700)
    assert.equal(await refreshButton.locator('[data-pc-section="loadingicon"]').count(), 0,
      `${name}: manual refresh spinner did not stop`)
    const continuityResult = await continuity.evaluate(state => {
      cancelAnimationFrame(state.frame)
      return { frames: state.frames, hiddenFrames: state.hiddenFrames, replacedFrames: state.replacedFrames }
    })
    await continuity.dispose()
    collectionDelay = 0
    assert.ok(continuityResult.frames > 2, `${name}: label continuity was not sampled across rendered frames`)
    assert.equal(continuityResult.replacedFrames, 0, `${name}: polling recreated traffic labels or their contents`)
    assert.equal(continuityResult.hiddenFrames, 0, `${name}: polling briefly hid visible traffic labels`)
    await assertLocalLabelLayout(page, name)
    await page.screenshot({ path: join(output, `${name}-close.png`), fullPage: true })
    const savedDistance = Number(await canvas.getAttribute('data-globe-distance'))
    const savedAzimuth = Number(await canvas.getAttribute('data-globe-azimuth'))
    await page.waitForTimeout(600)
    await page.reload()
    await page.locator('.globe-stage canvas').waitFor()
    await page.locator('.node-row').first().waitFor()
    await page.waitForTimeout(600)
    assert.ok(Math.abs(Number(await canvas.getAttribute('data-globe-distance')) - savedDistance) < 0.02,
      `${name}: zoom did not survive page reload`)
    assert.ok(Math.abs(Number(await canvas.getAttribute('data-globe-azimuth')) - savedAzimuth) < 0.02,
      `${name}: camera orientation did not survive page reload`)
    assert.equal(await page.getByRole('button', { name: 'Auto Rotate', exact: true }).count(), 1,
    `${name}: rotation switch did not survive page reload`)
    assert.equal(await page.locator('.node-row').filter({ hasText: 'Auckland' }).getAttribute('aria-pressed'), 'true',
      `${name}: selected node did not survive page reload`)
    await page.getByRole('button', { name: 'Reset View', exact: true }).click()
    await page.waitForFunction(longitude =>
      Math.abs(Number(document.querySelector('.globe-stage canvas')?.getAttribute('data-globe-azimuth')) - longitude) < 1e-8,
    defaultLongitude)
    assert.ok(Math.abs(Number(await canvas.getAttribute('data-globe-azimuth')) - defaultLongitude) < 1e-8,
      `${name}: reset view did not center on Hong Kong`)
    await page.locator('.node-row').filter({ hasText: 'Auckland' }).click()
    await page.reload()
    await page.locator('.node-row').first().waitFor()
    await page.locator('.globe-stage canvas').waitFor()
    assert.equal(await page.locator('.node-row.selected').count(), 0,
      `${name}: canceled selection returned after page reload`)
    assert.equal(await page.locator('.node-detail').count(), 0)
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 1)
    assert.equal(overflow, false, `${name}: horizontal overflow`)
    await page.screenshot({ path: join(output, `${name}.png`), fullPage: true })
    console.log(`${name}: ${landPixels} continent pixels; Hong Kong view, selection toggle, silent polling, continuous RTT labels, rotation, zoom LOD and layout passed`)
  }
  jitter = true
  await page.waitForTimeout(5000)
  assert.equal(await page.locator('.node-row').count(), 4, 'Transient collection errors removed buffered nodes')
  assert.equal(await page.locator('.dashboard-error').count(), 0,
    'Short collection errors must not display an incomplete-data warning')
  assert.ok(await page.locator('.node-row.stale').count() > 0, 'Buffered nodes must be marked stale')
  jitter = false
  await page.waitForTimeout(5000)
  assert.equal(await page.locator('.node-row.stale').count(), 0, 'Recovered node data remained stale')
  const recoveredRequest = page.waitForResponse(response =>
    response.url().includes(`/machines/${uuidString(2)}/networks/info`) && response.status() === 200,
  { timeout: 15_000 })
  hangNext = true
  await page.waitForTimeout(2500)
  assert.ok(hungRoute, 'The hanging-request fixture did not run')
  assert.equal(await page.locator('.node-row').count(), 4, 'An in-flight request removed buffered nodes')
  await recoveredRequest
  await page.waitForTimeout(300)
  assert.equal(await page.locator('.node-row.stale').count(), 0, 'A timed-out request did not recover through retry')
  await hungRoute.abort()
  colocated = true
  trafficScale = 0.1
  await page.reload()
  await page.locator('.node-row').first().waitFor()
  await page.waitForTimeout(3500)
  await page.locator('.node-row .node-name').filter({ hasText: 'Shanghai' }).click()
  const overlappingCanvas = page.locator('.globe-stage canvas')
  const overlappingBounds = await overlappingCanvas.boundingBox()
  await page.mouse.move(overlappingBounds.x + overlappingBounds.width / 2,
    overlappingBounds.y + overlappingBounds.height / 2)
  await page.mouse.wheel(0, -2500)
  await page.waitForFunction(() =>
    document.querySelector('.globe-labels')?.getAttribute('data-traffic-labels-visible') === 'true')
  await page.waitForFunction(() => [...document.querySelectorAll('.globe-traffic-label')]
    .some(element => getComputedStyle(element).display !== 'none'))
  assert.equal(await page.locator('.globe-stage canvas').getAttribute('data-globe-invalid-flows'), '0',
    'Colocated nodes generated invalid connection flow coordinates')
  const overlapping = JSON.parse(await page.locator('.globe-stage canvas').getAttribute('data-globe-flow-timings'))
  assert.equal(overlapping.length, 12, 'Overlapping device paths merged independent directions')
  assert.equal(new Set(overlapping.map(flow => flow.key)).size, 12, 'Distinct devices share an emitter key')
  const forwardOverlapping = overlapping.filter(flow => JSON.parse(flow.key)[2] === false)
  assert.ok(forwardOverlapping.every(flow => flow.emissionsPerSecond < 2),
    'Two-megabit overlapping links became solid streams')
  assert.ok(new Set(forwardOverlapping.map(flow => flow.travelSeconds)).size >= 3,
    'Overlapping paths lost their individual RTT travel speeds')
  const overlappingLabels = await assertLocalLabelLayout(page, 'overlapping mobile')
  assert.ok(overlappingLabels.some(label => label.traffic),
    'Crowded-link validation did not display any traffic labels')
  assert.equal(await page.locator('.globe-node-stack:visible').count(), 1,
    'Devices in the same city split into several scattered name groups')
  assert.equal(await page.locator('.globe-node-stack .globe-node-label').count(), 4,
    'A city group lost individual device names')
  const routeStack = page.locator('.globe-traffic-stack:visible')
  assert.equal(await routeStack.count(), 1, 'Overlapping geographic links split into separate label groups')
  assert.equal(await routeStack.locator('.globe-traffic-label').count(), 6,
    'A route group merged or discarded the separate device-link measurements')
  assert.ok(await routeStack.evaluate(element => element.scrollHeight > element.clientHeight),
    'Crowded mobile route groups should scroll internally')
  const distanceBeforeScrolling = await overlappingCanvas.getAttribute('data-globe-distance')
  await routeStack.hover()
  await page.mouse.wheel(0, 1200)
  await page.waitForFunction(() => {
    const stack = [...document.querySelectorAll('.globe-traffic-stack')]
      .find(element => getComputedStyle(element).display !== 'none')
    return stack?.scrollTop > 0
  })
  assert.ok(await routeStack.evaluate(element => element.scrollTop > 0),
    'The grouped route column cannot be scrolled')
  assert.equal(await overlappingCanvas.getAttribute('data-globe-distance'), distanceBeforeScrolling,
    'Scrolling a label column also zoomed the globe')
  await page.screenshot({ path: join(output, 'mobile-overlapping.png'), fullPage: true })
  await page.setViewportSize({ width: 1440, height: 1000 })
  await page.waitForFunction(() => document.querySelector('.globe-stage canvas')?.clientWidth > 800)
  await page.waitForTimeout(300)
  await routeStack.evaluate(element => { element.scrollTop = 0 })
  const desktopStacks = await assertLocalLabelLayout(page, 'overlapping desktop')
  const expandedRoutes = desktopStacks.find(stack => stack.traffic)
  assert.ok(expandedRoutes && expandedRoutes.height > expandedRoutes.rows[0].height * 2,
    'The desktop route group did not expand to multiple stacked rows')
  assert.equal(await page.locator('.globe-node-stack:visible').count(), 1)
  assert.equal(await routeStack.locator('.globe-traffic-label').count(), 6)
  await page.screenshot({ path: join(output, 'desktop-overlapping.png'), fullPage: true })
  empty = true
  await page.goto(dashboardUrl)
  await page.reload()
  await page.locator('.node-empty').waitFor()
  assert.equal(await page.locator('.node-row').count(), 0)
  assert.deepEqual(errors, [])
  console.log(`Empty state and browser errors passed. Screenshots: ${output}`)
} catch (error) {
  await page?.screenshot({ path: join(output, 'failure.png'), fullPage: true }).catch(() => {})
  throw error
} finally {
  await browser.close()
}
