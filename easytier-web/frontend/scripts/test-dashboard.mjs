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
let colocated = false
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
    country: locations[index].country,
    city: '',
    region: '',
    latitude: colocated ? 31.2 : locations[index].latitude,
    longitude: colocated ? 121.5 : locations[index].longitude,
  },
  routes: [0, 1, 2].filter(peer => peer !== index).map(peer => ({
    peer_id: peer + 1, inst_id: uuidString(peer + 11), cost: 1,
  })),
  peers: [0, 1, 2].filter(peer => peer !== index).map(peer => ({
    peer_id: peer + 1,
    conns: [{ conn_id: uuidString(peer + 101), is_closed: false, tunnel: { tunnel_type: 'udp' } }],
  })),
})

const browser = await chromium.launch({
  executablePath: process.env.CHROMIUM_EXECUTABLE || undefined,
  headless: true,
  args: ['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'],
})
const errors = []
try {
  const page = await browser.newPage()
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
      body = { info: { map: { [uuidString(id + 10)]: snapshot(id - 1) } } }
    }
    await route.fulfill({ contentType: 'application/json', body: JSON.stringify(body) })
  })
  await page.addInitScript(() => localStorage.setItem('lang', 'en'))
  const dashboardUrl = `${baseUrl}/#/h/${btoa(baseUrl)}`
  for (const [name, viewport] of [
    ['desktop', { width: 1440, height: 1000 }],
    ['mobile', { width: 390, height: 844 }],
  ]) {
    await page.setViewportSize(viewport)
    await page.goto(dashboardUrl)
    await page.reload()
    const canvas = page.locator('.globe-stage canvas')
    await canvas.waitFor()
    await page.locator('.node-row').first().waitFor()
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
    await page.locator('.node-row').filter({ hasText: 'Auckland' }).click()
    await assert.doesNotReject(() => page.locator('.node-detail').waitFor())
    assert.match(await page.locator('.node-detail').innerText(), /-36\.80/)
    assert.ok(await page.locator('.node-list').evaluate(element => element.clientHeight) > 40,
      `${name}: node details collapsed the node list`)
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
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2)
    await page.mouse.wheel(0, -2500)
    await page.waitForTimeout(300)
    assert.equal(await canvas.getAttribute('data-globe-cloud-level'), '2',
      `${name}: close zoom did not select the dense point cloud`)
    const stageBounds = await page.locator('.globe-stage').boundingBox()
    assert.ok(stageBounds.x + stageBounds.width <= viewport.width + 1,
      `${name}: globe stage exceeds the viewport`)
    await page.screenshot({ path: join(output, `${name}-close.png`), fullPage: true })
    await page.getByRole('button', { name: 'Reset View', exact: true }).click()
    await page.waitForTimeout(100)
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 1)
    assert.equal(overflow, false, `${name}: horizontal overflow`)
    await page.screenshot({ path: join(output, `${name}.png`), fullPage: true })
    console.log(`${name}: ${landPixels} continent pixels; rotation, zoom LOD, counts, selection and layout passed`)
  }
  colocated = true
  await page.reload()
  await page.locator('.node-row').first().waitFor()
  await page.waitForTimeout(800)
  assert.equal(await page.locator('.globe-stage canvas').getAttribute('data-globe-invalid-flows'), '0',
    'Colocated nodes generated invalid connection flow coordinates')
  empty = true
  await page.goto(dashboardUrl)
  await page.reload()
  await page.locator('.node-empty').waitFor()
  assert.equal(await page.locator('.node-row').count(), 0)
  assert.deepEqual(errors, [])
  console.log(`Empty state and browser errors passed. Screenshots: ${output}`)
} finally {
  await browser.close()
}
