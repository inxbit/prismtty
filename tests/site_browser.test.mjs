import { expect, test } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

test('loads the PrismTTY landing page without local runtime errors', async ({ page }) => {
  const messages = [];
  page.on('console', (message) => {
    if (message.type() === 'error' || message.type() === 'warning') {
      messages.push(`${message.type()}: ${message.text()}`);
    }
  });
  page.on('pageerror', (error) => messages.push(`pageerror: ${error.message}`));

  await page.goto('/');

  await expect(page).toHaveTitle('PrismTTY - Terminal Output Highlighting');
  await expect(page.getByRole('main')).toBeVisible();
  await expect(page.getByRole('heading', { level: 1 })).toBeVisible();
  expect(messages).toEqual([]);
});

test('shows the approved hero inside the first desktop viewport', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.goto('/');

  await expect(page.getByRole('heading', { level: 1, name: 'Noise becomes signal.' })).toBeVisible();
  const installLink = page.locator('.hero').getByRole('link', { name: 'Install', exact: true });
  await expect(installLink).toBeVisible();
  await expect(page.locator('[data-terminal] .tline')).not.toHaveCount(0);

  const installBox = await installLink.boundingBox();
  expect(installBox).not.toBeNull();
  expect(installBox.y + installBox.height).toBeLessThanOrEqual(900);
});

test('profile tabs implement roving keyboard focus and update one tabpanel', async ({ page }) => {
  await page.goto('/#profiles');
  const cisco = page.getByRole('tab', { name: 'cisco' });
  const juniper = page.getByRole('tab', { name: 'juniper' });
  const linuxUnix = page.getByRole('tab', { name: 'linux-unix' });
  const panel = page.getByRole('tabpanel');

  const expectProfileState = async (tab, tabId) => {
    await expect(tab).toBeFocused();
    await expect(tab).toHaveAttribute('aria-selected', 'true');
    await expect(tab).toHaveAttribute('tabindex', '0');
    await expect(page.locator('[data-profile-tab][aria-selected="true"]')).toHaveCount(1);
    await expect(page.locator('[data-profile-tab][tabindex="0"]')).toHaveCount(1);
    await expect(panel).toHaveAttribute('aria-labelledby', tabId);
  };

  await cisco.focus();
  await cisco.press('ArrowLeft');
  await expectProfileState(linuxUnix, 'profile-tab-linux-unix');

  await linuxUnix.press('ArrowRight');
  await expectProfileState(cisco, 'profile-tab-cisco');

  await cisco.press('End');
  await expectProfileState(linuxUnix, 'profile-tab-linux-unix');

  await linuxUnix.press('Home');
  await expectProfileState(cisco, 'profile-tab-cisco');

  await cisco.press('ArrowRight');
  await expectProfileState(juniper, 'profile-tab-juniper');
  await expect(panel).toContainText('ge-0/0/0');
  await expect(page.locator('[data-profile-body]')).not.toHaveAttribute('aria-live');
});

test('profile fallback keeps terminal output but hides inert tabs without JavaScript', async ({ browser }) => {
  const context = await browser.newContext({
    javaScriptEnabled: false,
    viewport: { width: 390, height: 844 },
  });
  const page = await context.newPage();

  try {
    await page.goto('/#profiles');
    await expect(page.locator('.profile-tabs')).toBeHidden();
    await expect(page.getByRole('tab')).toHaveCount(0);
    await expect(page.getByRole('tabpanel')).toHaveCount(1);
    await expect(page.getByRole('tabpanel')).toHaveAttribute('aria-labelledby', 'profile-tab-cisco');
    await expect(page.locator('[data-profile-title]')).toHaveText('ptty ssh edge-sw1.example.net');
    await expect(page.locator('[data-profile-body] .tline')).toHaveCount(3);
    await expect(page.locator('[data-profile-body]')).toContainText('GigabitEthernet1/0/1');
  } finally {
    await context.close();
  }
});

test('profile terminal preserves columns in a contained focusable scroll region', async ({ page }) => {
  await page.setViewportSize({ width: 320, height: 844 });
  await page.goto('/#profiles');
  const body = page.locator('[data-profile-body]');

  await expect(body).toHaveAttribute('tabindex', '0');
  await expect(body).toHaveAttribute('aria-label', 'Profile terminal output');
  await expect(body).toHaveCSS('white-space', 'pre');
  await expect(body).toHaveCSS('word-break', 'normal');

  const widths = await body.evaluate((element) => ({
    client: element.clientWidth,
    scroll: element.scrollWidth,
    viewport: window.innerWidth,
    document: document.documentElement.scrollWidth,
    body: document.body.scrollWidth,
  }));
  expect(widths.scroll).toBeGreaterThan(widths.client);
  expect(widths.document).toBe(widths.viewport);
  expect(widths.body).toBe(widths.viewport);

  await body.focus();
  await expect(body).toBeFocused();

  const results = await new AxeBuilder({ page })
    .include('#profiles')
    .withRules('scrollable-region-focusable')
    .analyze();
  expect(results.violations).toEqual([]);
});

test('comparison reports highlighted output at boundary positions', async ({ page }) => {
  await page.goto('/#compare');
  const range = page.getByRole('slider', { name: 'Highlighted output reveal' });
  const rangeElement = page.locator('[data-compare-range]');
  const output = page.locator('[data-compare-output]');
  const root = page.locator('[data-compare]');
  const rawButton = page.locator('[data-compare-mode="raw"]');
  const highlightedButton = page.locator('[data-compare-mode="highlighted"]');
  const softExpect = expect.configure({ soft: true, timeout: 1_000 });

  await range.fill('72');
  await softExpect(output).toHaveText('28% highlighted');
  await softExpect(rangeElement).toHaveAttribute('aria-valuetext', '28% highlighted');
  await expect(root).toHaveCSS('--compare-position', '72%');
  await expect(rawButton).toHaveAttribute('aria-pressed', 'false');
  await expect(highlightedButton).toHaveAttribute('aria-pressed', 'false');

  await page.setViewportSize({ width: 390, height: 844 });
  await expect(rawButton).toBeVisible();
  await expect(highlightedButton).toBeVisible();
  await softExpect(output).toHaveText('28% highlighted');

  await rawButton.click();
  await expect(root).toHaveCSS('--compare-position', '100%');
  await softExpect(output).toHaveText('0% highlighted');
  await softExpect(rangeElement).toHaveAttribute('aria-valuetext', '0% highlighted');
  await expect(rawButton).toHaveAttribute('aria-pressed', 'true');
  await expect(highlightedButton).toHaveAttribute('aria-pressed', 'false');

  await highlightedButton.click();
  await expect(root).toHaveCSS('--compare-position', '0%');
  await softExpect(output).toHaveText('100% highlighted');
  await softExpect(rangeElement).toHaveAttribute('aria-valuetext', '100% highlighted');
  await expect(rawButton).toHaveAttribute('aria-pressed', 'false');
  await expect(highlightedButton).toHaveAttribute('aria-pressed', 'true');
});

test('comparison keeps pressed state aligned across viewport changes', async ({ page }) => {
  await page.goto('/#compare');
  const range = page.getByRole('slider', { name: 'Highlighted output reveal' });
  const rawButton = page.locator('[data-compare-mode="raw"]');
  const highlightedButton = page.locator('[data-compare-mode="highlighted"]');

  await range.fill('72');
  await expect(rawButton).toHaveAttribute('aria-pressed', 'false');
  await expect(highlightedButton).toHaveAttribute('aria-pressed', 'false');

  await page.setViewportSize({ width: 390, height: 844 });
  await expect(rawButton).toHaveAttribute('aria-pressed', 'false');
  await expect(highlightedButton).toHaveAttribute('aria-pressed', 'false');
  await rawButton.click();
  await expect(rawButton).toHaveAttribute('aria-pressed', 'true');
  await expect(highlightedButton).toHaveAttribute('aria-pressed', 'false');
  await highlightedButton.click();
  await expect(rawButton).toHaveAttribute('aria-pressed', 'false');
  await expect(highlightedButton).toHaveAttribute('aria-pressed', 'true');

  await page.setViewportSize({ width: 1440, height: 900 });
  await range.fill('40');
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(rawButton).toHaveAttribute('aria-pressed', 'false');
  await expect(highlightedButton).toHaveAttribute('aria-pressed', 'false');
});

test('comparison keeps raw and highlighted pane scrolling aligned', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto('/#compare');
  const raw = page.locator('[data-compare-raw]');
  const highlighted = page.locator('[data-compare-hl]');
  const softExpect = expect.configure({ soft: true, timeout: 1_000 });

  const rawTarget = await raw.evaluate((pane) => {
    const target = Math.min(120, pane.scrollWidth - pane.clientWidth);
    pane.scrollLeft = target;
    return target;
  });
  expect(rawTarget).toBeGreaterThan(0);
  await softExpect.poll(() => highlighted.evaluate((pane) => pane.scrollLeft)).toBe(rawTarget);

  const highlightedTarget = await highlighted.evaluate((pane) => {
    const target = Math.min(40, pane.scrollWidth - pane.clientWidth);
    pane.scrollLeft = target;
    return target;
  });
  expect(highlightedTarget).toBeGreaterThan(0);
  await softExpect.poll(() => raw.evaluate((pane) => pane.scrollLeft)).toBe(highlightedTarget);
});

test('comparison fallback hides inert controls without JavaScript', async ({ browser }) => {
  const context = await browser.newContext({
    javaScriptEnabled: false,
    viewport: { width: 1440, height: 900 },
  });
  const page = await context.newPage();
  const softExpect = expect.configure({ soft: true, timeout: 1_000 });

  try {
    await page.goto('/#compare');
    await expect(page.locator('[data-compare-raw] .tline')).toHaveCount(3);
    await expect(page.locator('[data-compare-hl] .tline')).toHaveCount(3);
    await softExpect(page.locator('.compare-control')).toBeHidden();
    await softExpect(page.getByRole('slider', { name: 'Highlighted output reveal' })).toHaveCount(0);

    await page.setViewportSize({ width: 390, height: 844 });
    await softExpect(page.locator('.compare-mobile-controls')).toBeHidden();
    await softExpect(page.getByRole('button', { name: 'Show raw output' })).toHaveCount(0);
    await softExpect(page.getByRole('button', { name: 'Show PrismTTY output' })).toHaveCount(0);
  } finally {
    await context.close();
  }
});

test('has no unfocusable scrollable regions on mobile', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto('/');

  const results = await new AxeBuilder({ page })
    .withRules('scrollable-region-focusable')
    .analyze();

  expect(results.violations).toEqual([]);
});
