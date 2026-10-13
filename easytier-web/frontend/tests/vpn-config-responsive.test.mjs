import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdir, mkdtemp } from 'node:fs/promises';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { test } from 'node:test';
import { chromium } from 'playwright';

const testDir = dirname(fileURLToPath(import.meta.url));
const frontendRoot = resolve(testDir, '..');
const frontendLibRoot = resolve(frontendRoot, '../frontend-lib');
const viteCli = resolve(frontendRoot, 'node_modules/vite/bin/vite.js');
const viteConfig = resolve(frontendLibRoot, 'tests/browser/vite.config.ts');
const browserEntry = '/tests/browser/';

async function availablePort() {
  const server = createServer();
  await new Promise((done, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', done);
  });
  const port = server.address().port;
  await new Promise((done, reject) => server.close(error => error ? reject(error) : done()));
  return port;
}

async function waitForServer(url, child, logs) {
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`Vite exited early:\n${logs()}`);
    try {
      const response = await fetch(url);
      if (response.ok) return;
    } catch {
      // The server is still starting.
    }
    await delay(100);
  }
  throw new Error(`Timed out waiting for Vite at ${url}:\n${logs()}`);
}

async function assertNoHorizontalOverflow(page, width, state) {
  const dimensions = await page.evaluate(() => ({
    innerWidth: window.innerWidth,
    documentWidth: document.documentElement.scrollWidth,
    bodyWidth: document.body.scrollWidth,
  }));
  assert.ok(
    dimensions.documentWidth <= width + 1 && dimensions.bodyWidth <= width + 1,
    `${state} at ${width}px must not overflow horizontally: ${JSON.stringify(dimensions)}`,
  );
}

test('VPN configuration remains editable without horizontal overflow at mobile, tablet, and desktop widths', async () => {
  const port = await availablePort();
  const url = `http://127.0.0.1:${port}${browserEntry}`;
  let serverLogs = '';
  const server = spawn(process.execPath, [viteCli, '--config', viteConfig, '--host', '127.0.0.1', '--port', String(port), '--strictPort'], {
    cwd: frontendLibRoot,
    windowsHide: true,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  server.stdout.on('data', chunk => { serverLogs += chunk.toString(); });
  server.stderr.on('data', chunk => { serverLogs += chunk.toString(); });

  let browser;
  try {
    await waitForServer(url, server, () => serverLogs);
    const screenshotDir = process.env.VPN_CONFIG_SCREENSHOT_DIR
      ?? await mkdtemp(join(tmpdir(), 'easytier-vpn-ui-'));
    await mkdir(screenshotDir, { recursive: true });

    browser = await chromium.launch({ headless: true });
    const page = await browser.newPage({ viewport: { width: 360, height: 960 } });
    const pageErrors = [];
    const consoleErrors = [];
    page.on('pageerror', error => pageErrors.push(error.message));
    page.on('console', message => {
      if (message.type() === 'error') consoleErrors.push(message.text());
    });
    await page.goto(url, { waitUntil: 'domcontentloaded', timeout: 60_000 });
    try {
      await page.locator('#app .frontend-lib').waitFor({ state: 'attached', timeout: 30_000 });
    } catch (error) {
      throw new Error(`${error}\nBrowser errors: ${consoleErrors.join('\n')}\nVite logs:\n${serverLogs}`);
    }
    await page.getByRole('button', { name: '高级设置', exact: true }).click();
    await page.locator('#vpn_exit_enabled').waitFor({ state: 'visible' });
    await page.getByText('分流规则', { exact: true }).waitFor({ state: 'visible' });

    await page.getByRole('button', { name: '添加规则', exact: true }).click();
    await page.getByRole('button', { name: '添加规则', exact: true }).click();
    await page.getByRole('button', { name: '添加规则', exact: true }).click();
    await page.getByRole('button', { name: '添加规则', exact: true }).click();

    const cards = page.getByTestId('vpn-rule-card');
    assert.equal(await cards.count(), 4, 'four new blank rules should be editable');
    await page.locator('#vpn_rule_cidr_0').fill('0.0.0.0/0');
    await page.locator('#vpn_rule_cidr_1').fill('2001:db8::/32');
    await page.locator('#vpn_rule_priority_2').fill('20');
    await page.locator('#vpn_rule_priority_3').fill('30');

    const exitIps = Array.from({ length: 12 }, (_, index) => index % 2 === 0
      ? `2001:db8:1234:5678:9abc:def0:1234:${(index + 1).toString(16)}`
      : `10.${index}.20.${index + 1}`);
    const secondRule = cards.nth(1);
    for (const [index, ip] of exitIps.entries()) {
      await secondRule.getByRole('button', { name: '添加出口 IP', exact: true }).click();
      await secondRule.getByRole('textbox', { name: `出口 IP ${index + 1}`, exact: true }).fill(ip);
    }
    await secondRule.getByTestId('vpn-exit-row').nth(11)
      .getByRole('button', { name: '上移出口 IP', exact: true }).click();
    await secondRule.getByTestId('vpn-exit-row').last()
      .getByRole('button', { name: '删除出口 IP', exact: true }).click();
    const expectedExitOrder = [...exitIps.slice(0, 10), exitIps[11]];
    assert.deepEqual(
      await secondRule.locator('input[aria-label^="出口 IP "]').evaluateAll(inputs => inputs.map(input => input.value)),
      expectedExitOrder,
      'exit IP entries must remain editable and ordered after a move and delete',
    );

    await cards.nth(3).getByRole('button', { name: '上移规则', exact: true }).click();
    await cards.nth(1).getByRole('button', { name: '上移规则', exact: true }).click();
    const afterReorder = page.getByTestId('vpn-rule-card');
    assert.deepEqual(
      await afterReorder.locator('input[id^="vpn_rule_cidr_"]').evaluateAll(inputs => inputs.map(input => input.value)),
      ['2001:db8::/32', '0.0.0.0/0', '', ''],
      'rule controls and exit lists must follow their rules when reordered',
    );
    assert.deepEqual(
      await afterReorder.locator('input[id^="vpn_rule_priority_"]').evaluateAll(inputs => inputs.map(input => input.value)),
      ['', '', '30', '20'],
    );
    assert.equal(
      await afterReorder.nth(0).locator('input[aria-label^="出口 IP "]').first().isDisabled(),
      false,
      'the reordered via-exit rule should keep its long IP list editable',
    );
    await afterReorder.nth(2).getByRole('button', { name: '删除规则', exact: true }).click();
    assert.equal(await page.getByTestId('vpn-rule-card').count(), 3, 'a blank rule should be removable');
    assert.equal(await page.locator('#vpn_rule_cidr_0').inputValue(), '2001:db8::/32');
    assert.equal(await page.locator('#vpn_rule_cidr_2').inputValue(), '');

    const widths = [360, 768, 1280];
    for (const [index, width] of widths.entries()) {
      await page.setViewportSize({ width, height: 960 });
      await page.waitForTimeout(100);
      await assertNoHorizontalOverflow(page, width, 'rule editor');

      const existingCount = await page.getByTestId('vpn-rule-card').count();
      await page.getByRole('button', { name: '添加规则', exact: true }).click();
      const newRule = page.getByTestId('vpn-rule-card').nth(existingCount);
      const newCidr = `2001:db8:${width}::/48`;
      await newRule.locator('input[id^="vpn_rule_cidr_"]').fill(newCidr);
      await newRule.locator('input[id^="vpn_rule_priority_"]').fill('7');
      for (const [exitIndex, exit] of ['10.144.0.2', '2001:db8::3'].entries()) {
        await newRule.getByRole('button', { name: '添加出口 IP', exact: true }).click();
        await newRule.getByRole('textbox', { name: `出口 IP ${exitIndex + 1}`, exact: true }).fill(exit);
      }
      await newRule.getByTestId('vpn-exit-row').nth(1)
        .getByRole('button', { name: '上移出口 IP', exact: true }).click();
      assert.deepEqual(
        await newRule.locator('input[aria-label^="出口 IP "]').evaluateAll(inputs => inputs.map(input => input.value)),
        ['2001:db8::3', '10.144.0.2'],
        `exit reordering must work at ${width}px`,
      );
      await newRule.getByTestId('vpn-exit-row').last()
        .getByRole('button', { name: '删除出口 IP', exact: true }).click();
      await assertNoHorizontalOverflow(page, width, 'new rule and edited exits');

      await newRule.getByRole('button', { name: '上移规则', exact: true }).click();
      const movedRule = page.getByTestId('vpn-rule-card').nth(existingCount - 1);
      assert.equal(await movedRule.locator('input[id^="vpn_rule_cidr_"]').inputValue(), newCidr);
      assert.equal(await movedRule.getByRole('textbox', { name: '出口 IP 1', exact: true }).inputValue(), '2001:db8::3');
      await movedRule.getByRole('button', { name: '下移规则', exact: true }).click();
      await page.getByTestId('vpn-rule-card').last()
        .getByRole('button', { name: '删除规则', exact: true }).click();
      assert.equal(await page.getByTestId('vpn-rule-card').count(), existingCount);
      await assertNoHorizontalOverflow(page, width, 'rule reordered and deleted');

      const action = page.locator('#vpn_rule_action_1');
      await action.click();
      await page.getByRole('option', { name: '始终直连', exact: true }).waitFor({ state: 'visible' });
      await assertNoHorizontalOverflow(page, width, 'open action selector');
      if (index === 0) {
        await page.screenshot({
          path: join(screenshotDir, 'vpn-config-360-action-open.png'),
          fullPage: false,
          animations: 'disabled',
        });
        await page.getByRole('option', { name: '始终直连', exact: true }).click();
        await assertNoHorizontalOverflow(page, width, 'selected action');
      } else {
        await page.keyboard.press('Escape');
      }

      await page.screenshot({
        path: join(screenshotDir, `vpn-config-${width}.png`),
        fullPage: true,
        animations: 'disabled',
      });
    }

    assert.deepEqual(
      [...pageErrors, ...consoleErrors],
      [],
      `browser runtime errors: ${[...pageErrors, ...consoleErrors].join('; ')}`,
    );
    console.log(`VPN config screenshots: ${screenshotDir}`);
  } finally {
    await browser?.close();
    server.kill();
    if (server.exitCode === null) {
      await new Promise(resolveExit => server.once('exit', resolveExit));
    }
  }
});
