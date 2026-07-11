import { expect, test } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

async function commandRowGeometry(page) {
  return page.locator('.command-row').evaluate((row) => {
    const rowBox = row.getBoundingClientRect();
    const buttonBox = row.querySelector('[data-copy-command]').getBoundingClientRect();
    return {
      rowWidth: rowBox.width,
      rowHeight: rowBox.height,
      buttonX: buttonBox.x - rowBox.x,
      buttonY: buttonBox.y - rowBox.y,
      buttonWidth: buttonBox.width,
      buttonHeight: buttonBox.height,
    };
  });
}

async function settleFiniteAnimations(page) {
  await page.evaluate(async () => {
    const finite = document.getAnimations().filter((animation) => {
      const endTime = animation.effect?.getComputedTiming().endTime;
      return Number.isFinite(endTime);
    });
    await Promise.all(finite.map((animation) => animation.finished.catch(() => {})));
  });
}

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

test('motion preference changes make the hero complete and stable without a reload', async ({ page }) => {
  await page.emulateMedia({ reducedMotion: 'no-preference' });
  await page.goto('/');
  const terminal = page.locator('[data-terminal]');
  const comparison = page.locator('[data-compare]');

  await expect(terminal).not.toContainText('%LINK-3-UPDOWN');
  await page.emulateMedia({ reducedMotion: 'reduce' });

  await expect(terminal).toContainText('%LINK-3-UPDOWN', { timeout: 1500 });
  const firstStableMarkup = await terminal.innerHTML();
  await page.waitForTimeout(500);
  expect(await terminal.innerHTML()).toBe(firstStableMarkup);
  await expect(page.locator('.section').first()).toHaveCSS('opacity', '1');
  await expect(page.locator('.section').first()).toHaveCSS('transform', 'none');
  await expect(comparison).toHaveCSS('--compare-position', '50%');

  await page.emulateMedia({ reducedMotion: 'no-preference' });
  await expect.poll(() => terminal.innerHTML()).not.toBe(firstStableMarkup);

  await page.emulateMedia({ reducedMotion: 'reduce' });
  await expect(terminal).toContainText('%LINK-3-UPDOWN', { timeout: 1500 });
  const secondStableMarkup = await terminal.innerHTML();
  await page.waitForTimeout(500);
  expect(await terminal.innerHTML()).toBe(secondStableMarkup);
});

test('hero playback stops offscreen and resumes when the preview returns', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.emulateMedia({ reducedMotion: 'no-preference' });
  await page.goto('/');
  const terminal = page.locator('[data-terminal]');

  await expect(terminal).not.toContainText('%LINK-3-UPDOWN');
  await page.locator('.site-footer').scrollIntoViewIfNeeded();
  await expect(terminal).toContainText('%LINK-3-UPDOWN', { timeout: 2000 });
  const offscreenMarkup = await terminal.innerHTML();
  await page.waitForTimeout(500);
  expect(await terminal.innerHTML()).toBe(offscreenMarkup);

  await page.locator('#preview').scrollIntoViewIfNeeded();
  await expect.poll(() => terminal.innerHTML()).not.toBe(offscreenMarkup);
});

test('hero playback stops while the document is hidden and resumes when visible', async ({ page }) => {
  await page.emulateMedia({ reducedMotion: 'no-preference' });
  await page.goto('/');
  const terminal = page.locator('[data-terminal]');

  await page.evaluate(() => {
    window.__prismttyTestHidden = true;
    Object.defineProperty(document, 'hidden', {
      configurable: true,
      get: () => window.__prismttyTestHidden,
    });
    document.dispatchEvent(new Event('visibilitychange'));
  });
  await expect(terminal).toContainText('%LINK-3-UPDOWN');
  const hiddenMarkup = await terminal.innerHTML();
  await page.waitForTimeout(500);
  expect(await terminal.innerHTML()).toBe(hiddenMarkup);

  await page.evaluate(() => {
    window.__prismttyTestHidden = false;
    document.dispatchEvent(new Event('visibilitychange'));
  });
  await expect.poll(() => terminal.innerHTML()).not.toBe(hiddenMarkup);
});

test('missing IntersectionObserver falls back to complete static content', async ({ page }) => {
  await page.addInitScript(() => {
    Reflect.deleteProperty(window, 'IntersectionObserver');
  });
  const errors = [];
  page.on('pageerror', (error) => errors.push(error.message));
  await page.goto('/');
  const terminal = page.locator('[data-terminal]');

  await expect(terminal).toContainText('%LINK-3-UPDOWN', { timeout: 1500 });
  const stableMarkup = await terminal.innerHTML();
  await page.waitForTimeout(500);
  expect(await terminal.innerHTML()).toBe(stableMarkup);
  for (const selector of ['.proof-rail', '.command-band', '.section']) {
    await expect(page.locator(selector).first()).toHaveCSS('opacity', '1');
    await expect(page.locator(selector).first()).toHaveCSS('transform', 'none');
  }
  expect(errors).toEqual([]);
});

test('sections remain rendered before their reveal intersection', async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.emulateMedia({ reducedMotion: 'no-preference' });
  await page.goto('/');
  const install = page.locator('#install');

  const geometry = await install.boundingBox();
  expect(geometry).not.toBeNull();
  expect(geometry.y).toBeGreaterThan(900);
  await expect(install).toHaveCSS('opacity', '1');
  await expect(install).toHaveCSS('transform', 'none');
  await expect(install.getByRole('heading', { name: 'One command away.' })).toBeVisible();
});

test('user comparison state survives live motion preference changes', async ({ page }) => {
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await page.goto('/#compare');
  const comparison = page.locator('[data-compare]');
  const range = page.getByRole('slider', { name: 'Highlighted output reveal' });

  await expect(comparison).toHaveCSS('--compare-position', '50%');
  await range.fill('72');
  await expect(comparison).toHaveCSS('--compare-position', '72%');
  await expect(comparison).toHaveAttribute('data-compare-source', 'user');

  await page.emulateMedia({ reducedMotion: 'no-preference' });
  await page.locator('[data-compare-step]').first().scrollIntoViewIfNeeded();
  await page.waitForTimeout(250);
  await expect(comparison).toHaveCSS('--compare-position', '72%');

  await page.emulateMedia({ reducedMotion: 'reduce' });
  await expect(comparison).toHaveCSS('--compare-position', '72%');
});

test('profile and hero entrance motion use only opacity and transforms', async ({ page }) => {
  await page.emulateMedia({ reducedMotion: 'no-preference' });
  await page.goto('/#profiles');
  const body = page.locator('[data-profile-body]');

  await page.getByRole('tab', { name: 'juniper' }).click();
  const keyframeProperties = await body.evaluate((element) => {
    const allowedMetadata = new Set(['offset', 'computedOffset', 'easing', 'composite']);
    return element.getAnimations().flatMap((animation) => (
      animation.effect.getKeyframes().flatMap((frame) => (
        Object.keys(frame).filter((property) => !allowedMetadata.has(property))
      ))
    ));
  });
  expect(keyframeProperties.length).toBeGreaterThan(0);
  expect(new Set(keyframeProperties)).toEqual(new Set(['opacity', 'transform']));
  await expect(page.locator('.hero')).toHaveClass(/hero-enter/);

  await page.emulateMedia({ reducedMotion: 'reduce' });
  await page.getByRole('tab', { name: 'fortinet' }).click();
  expect(await body.evaluate((element) => element.getAnimations().length)).toBe(0);
});

test('mobile menu traps focus and restores the trigger on Escape', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto('/');
  const trigger = page.locator('[data-menu-trigger]');
  const nav = page.getByRole('navigation', { name: 'Primary navigation' });
  const firstLink = nav.getByRole('link', { name: 'Demo' });
  const lastLink = nav.getByRole('link', { name: 'GitHub' });

  await trigger.click();
  await expect(trigger).toHaveAttribute('aria-expanded', 'true');
  await expect(trigger).toHaveAccessibleName('Close navigation');
  await expect(nav).toBeVisible();
  await expect(firstLink).toBeFocused();
  await settleFiniteAnimations(page);

  const overlayBox = await nav.boundingBox();
  expect(overlayBox).not.toBeNull();
  expect(overlayBox.x).toBeGreaterThanOrEqual(12);
  expect(overlayBox.y).toBeGreaterThanOrEqual(92);
  expect(overlayBox.x + overlayBox.width).toBeLessThanOrEqual(378);
  expect(overlayBox.y + overlayBox.height).toBeLessThanOrEqual(832);
  expect(overlayBox.height).toBeGreaterThanOrEqual(700);
  const linkBoxes = await nav.locator('a').evaluateAll((links) => links.map((link) => {
    const box = link.getBoundingClientRect();
    return { top: box.top, bottom: box.bottom };
  }));
  for (let index = 1; index < linkBoxes.length; index += 1) {
    expect(linkBoxes[index].top).toBeGreaterThanOrEqual(linkBoxes[index - 1].bottom);
  }

  await page.keyboard.press('Shift+Tab');
  await expect(trigger).toBeFocused();
  await page.keyboard.press('Shift+Tab');
  await expect(lastLink).toBeFocused();
  await page.keyboard.press('Tab');
  await expect(trigger).toBeFocused();

  await page.keyboard.press('Escape');
  await expect(trigger).toHaveAttribute('aria-expanded', 'false');
  await expect(trigger).toHaveAccessibleName('Open navigation');
  await expect(trigger).toBeFocused();
});

test('mobile menu keeps every destination inside a short landscape viewport', async ({ page }) => {
  await page.setViewportSize({ width: 667, height: 375 });
  await page.goto('/');
  const trigger = page.locator('[data-menu-trigger]');
  const nav = page.getByRole('navigation', { name: 'Primary navigation' });

  await trigger.click();
  await settleFiniteAnimations(page);

  const geometry = await nav.evaluate((element) => {
    const box = element.getBoundingClientRect();
    const lastLink = element.lastElementChild.getBoundingClientRect();
    return {
      navTop: box.top,
      navBottom: box.bottom,
      navClientHeight: element.clientHeight,
      navScrollHeight: element.scrollHeight,
      lastLinkTop: lastLink.top,
      lastLinkBottom: lastLink.bottom,
      viewportHeight: innerHeight,
      overflowY: getComputedStyle(element).overflowY,
    };
  });

  expect(geometry.navTop).toBeGreaterThanOrEqual(92);
  expect(geometry.navBottom).toBeLessThanOrEqual(geometry.viewportHeight - 12);
  expect(geometry.navScrollHeight).toBeLessThanOrEqual(geometry.navClientHeight);
  expect(geometry.lastLinkTop).toBeGreaterThanOrEqual(geometry.navTop);
  expect(geometry.lastLinkBottom).toBeLessThanOrEqual(geometry.navBottom);
  expect(geometry.overflowY).toBe('auto');
});

test('same-page mobile navigation closes before moving focus to its destination', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto('/');
  const trigger = page.locator('[data-menu-trigger]');
  const compareLink = page.locator('[data-site-nav] a[href="#compare"]');
  const destination = page.locator('#compare');

  await trigger.click();
  await compareLink.click();

  await expect(page).toHaveURL(/#compare$/);
  await expect(trigger).toHaveAttribute('aria-expanded', 'false');
  await expect(destination).toBeFocused();
  await expect(compareLink).toHaveAttribute('aria-current', 'location');
});

test('active navigation exposes one current location as sections change', async ({ page }) => {
  await page.goto('/#install');
  const nav = page.getByRole('navigation', { name: 'Primary navigation' });
  const installLink = nav.getByRole('link', { name: 'Install' });

  await expect(installLink).toHaveAttribute('aria-current', 'location');
  await expect(nav.locator('[aria-current="location"]')).toHaveCount(1);

  await page.locator('#profiles').scrollIntoViewIfNeeded();
  await expect(nav.getByRole('link', { name: 'Profiles' })).toHaveAttribute(
    'aria-current',
    'location',
  );
  await expect(installLink).not.toHaveAttribute('aria-current', 'location');
});

test('install methods update one pressed state and the visible fixed command', async ({ page }) => {
  await page.goto('/#install');
  const homebrew = page.getByRole('button', { name: 'Homebrew' });
  const cargo = page.getByRole('button', { name: 'Cargo' });
  const command = page.locator('[data-install-command]');

  await expect(homebrew).toHaveAttribute('aria-pressed', 'true');
  await expect(cargo).toHaveAttribute('aria-pressed', 'false');
  await expect(command).toHaveText('brew install inxbit/tap/prismtty');

  await cargo.click();
  await expect(homebrew).toHaveAttribute('aria-pressed', 'false');
  await expect(cargo).toHaveAttribute('aria-pressed', 'true');
  await expect(command).toHaveText('cargo install prismtty');
  await expect(page.locator('[data-install-method][aria-pressed="true"]')).toHaveCount(1);
});

test('copy success reports the visible command without shifting its controls', async ({ page, context }) => {
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await page.goto('/#install');
  await page.getByRole('button', { name: 'Cargo' }).click();
  const button = page.locator('[data-copy-command]');
  await page.evaluate(() => document.fonts.ready);
  await button.scrollIntoViewIfNeeded();
  const before = await commandRowGeometry(page);

  await button.click();

  await expect(page.locator('[data-copy-status]')).toHaveText('Command copied');
  await expect(button).toHaveText('Copied');
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe('cargo install prismtty');
  expect(await commandRowGeometry(page)).toEqual(before);
});

test('copy failure gives truthful manual-copy feedback without shifting controls', async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(navigator.clipboard, 'writeText', {
      configurable: true,
      value: async () => { throw new DOMException('Denied', 'NotAllowedError'); },
    });
  });
  await page.goto('/#install');
  const button = page.locator('[data-copy-command]');
  await page.evaluate(() => document.fonts.ready);
  await button.scrollIntoViewIfNeeded();
  const before = await commandRowGeometry(page);

  await button.click();

  await expect(page.locator('[data-copy-status]')).toHaveText(
    'Select the command and copy it manually',
  );
  await expect(button).toHaveText('Select command');
  expect(await commandRowGeometry(page)).toEqual(before);
});

test('pending clipboard writes keep the visible installation command stable', async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(navigator.clipboard, 'writeText', {
      configurable: true,
      value: (text) => {
        window.__copiedInstallCommand = text;
        return new Promise((resolve) => {
          window.__resolveInstallClipboard = resolve;
        });
      },
    });
  });
  await page.goto('/#install');
  const command = page.locator('[data-install-command]');
  const cargo = page.getByRole('button', { name: 'Cargo' });
  const copy = page.locator('[data-copy-command]');

  await copy.click();
  await expect(copy).toBeDisabled();
  await expect(copy).toHaveAttribute('aria-busy', 'true');
  await expect(cargo).toBeDisabled();
  await cargo.evaluate((button) => button.click());
  await expect(command).toHaveText('brew install inxbit/tap/prismtty');

  await page.evaluate(() => window.__resolveInstallClipboard());

  await expect(copy).toBeEnabled();
  await expect(copy).not.toHaveAttribute('aria-busy');
  await expect(cargo).toBeEnabled();
  await expect(page.locator('[data-copy-status]')).toHaveText('Command copied');
  expect(await page.evaluate(() => window.__copiedInstallCommand)).toBe(
    await command.textContent(),
  );
});

test('mobile fallback keeps navigation and Homebrew available without inert controls', async ({ browser }) => {
  const context = await browser.newContext({
    javaScriptEnabled: false,
    viewport: { width: 320, height: 844 },
  });
  const page = await context.newPage();

  try {
    await page.goto('/#install');
    const nav = page.getByRole('navigation', { name: 'Primary navigation' });
    await expect(nav).toBeVisible();
    for (const name of ['Demo', 'Compare', 'Profiles', 'Install', 'GitHub']) {
      await expect(nav.getByRole('link', { name })).toBeVisible();
    }
    await expect(page.getByRole('button', { name: 'Open navigation' })).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Homebrew' })).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Cargo' })).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Copy command' })).toHaveCount(0);
    await expect(page.locator('[data-install-command]')).toHaveText(
      'brew install inxbit/tap/prismtty',
    );
  } finally {
    await context.close();
  }
});

test('install command is a labelled focusable scroll region without page overflow', async ({ page }) => {
  for (const width of [320, 390]) {
    await page.setViewportSize({ width, height: 844 });
    await page.goto('/#install');
    const command = page.locator('[data-install-command]');

    await expect(command).toHaveAttribute('tabindex', '0');
    await expect(command).toHaveAttribute('role', 'region');
    await expect(command).toHaveAccessibleName('Installation command');
    await expect(command).toHaveCSS('white-space', 'nowrap');
    const widths = await command.evaluate((element) => ({
      client: element.clientWidth,
      scroll: element.scrollWidth,
      viewport: window.innerWidth,
      document: document.documentElement.scrollWidth,
      body: document.body.scrollWidth,
    }));
    expect(widths.scroll).toBeGreaterThan(widths.client);
    expect(widths.document).toBe(widths.viewport);
    expect(widths.body).toBe(widths.viewport);
    await command.focus();
    await expect(command).toBeFocused();

    await settleFiniteAnimations(page);
    const results = await new AxeBuilder({ page }).analyze();
    expect(results.violations).toEqual([]);
  }
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
