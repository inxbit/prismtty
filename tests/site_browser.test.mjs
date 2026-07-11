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
