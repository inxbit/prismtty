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

test('has no unfocusable scrollable regions on mobile', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto('/');

  const results = await new AxeBuilder({ page })
    .withRules('scrollable-region-focusable')
    .analyze();

  expect(results.violations).toEqual([]);
});
