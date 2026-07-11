import { expect, test } from '@playwright/test';

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
