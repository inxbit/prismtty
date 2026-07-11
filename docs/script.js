/* PrismTTY site — a miniature of what the tool does.
   One small rule-based highlighter feeds three surfaces:
   the animated hero terminal, the raw/highlighted compare slider,
   and the interactive profile tabs. No dependencies. */

(() => {
  'use strict';

  const reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  const ESC = { '&': '&amp;', '<': '&lt;', '>': '&gt;' };
  const esc = (s) => s.replace(/[&<>]/g, (c) => ESC[c]);
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const rand = (a, b) => a + Math.floor(Math.random() * (b - a));

  /* ---- the highlight rules (mirror PrismTTY's token families) ---- */
  const RULES = [
    { cls: 'sev', re: /%[A-Z][A-Z0-9_]*-\d+-[A-Z0-9_]+/g },
    {
      cls: 'iface',
      re: /\b(?:TenGigabitEthernet|GigabitEthernet|FastEthernet|Port-channel|Management|Loopback|Ethernet|Vlan|Mgmt|Gi|Te|Fa|Eth|Po|Lo|ge-|xe-|et-|fe-|ae|port\d+|em\d+|eth\d+|wg\d+)(?:[\d/.:]+)?\b/g,
    },
    { cls: 'ip', re: /\b\d{1,3}(?:\.\d{1,3}){3}(?:\/\d{1,2})?\b/g },
    { cls: 'up', re: /\b(?:up|connected|active|established|enabled)\b/gi },
    { cls: 'down', re: /\b(?:down|notconnect|disabled|err-disabled|inactive)\b/gi },
  ];

  // Apply rules to one line; first match wins on overlap (PrismTTY's exclusive model).
  function highlight(line) {
    const marks = [];
    for (const { cls, re } of RULES) {
      re.lastIndex = 0;
      let m;
      while ((m = re.exec(line))) {
        if (m[0].length === 0) { re.lastIndex++; continue; }
        marks.push({ start: m.index, end: m.index + m[0].length, cls });
      }
    }
    marks.sort((a, b) => a.start - b.start || b.end - a.end);

    let out = '';
    let pos = 0;
    for (const mk of marks) {
      if (mk.start < pos) continue;
      out += esc(line.slice(pos, mk.start));
      out += `<span class="hl-${mk.cls}">${esc(line.slice(mk.start, mk.end))}</span>`;
      pos = mk.end;
    }
    out += esc(line.slice(pos));
    return out;
  }

  // A "line" is either an output string or an input {p: prompt, c: command}.
  const blank = (s) => s === '' ? '&nbsp;' : s;

  function lineHTML(line) {
    if (typeof line === 'string') return `<span class="tline">${blank(highlight(line))}</span>`;
    return `<span class="tline"><span class="hl-host">${esc(line.p)}</span><span class="t-cmd">${esc(line.c)}</span></span>`;
  }
  function rawHTML(line) {
    const text = typeof line === 'string' ? line : line.p + line.c;
    return `<span class="tline">${blank(esc(text))}</span>`;
  }
  const render = (lines, fn) => lines.map(fn).join('');

  /* ---- sample sessions ---- */
  const HERO = [
    { p: 'ops@workstation%', c: ' ptty ssh edge-sw1.example.net' },
    'Connected to edge-sw1.example.net.',
    { p: 'edge-sw1#', c: 'show ip interface brief' },
    'Interface              IP-Address      Status    Protocol',
    'GigabitEthernet1/0/1   192.0.2.10      up        up',
    'GigabitEthernet1/0/2   198.51.100.42   down      down',
    'Vlan10                 203.0.113.1     up        up',
    { p: 'edge-sw1#', c: 'show logging | include LINK' },
    '%LINK-3-UPDOWN: Interface Gi1/0/2, changed state to down',
  ];

  const COMPARE = [
    { p: 'edge-sw1#', c: 'show interfaces status' },
    'Port      Name        Status       Vlan    Duplex  Speed Type',
    'Gi1/0/1   uplink-a    connected    trunk     full    1000 1000BaseTX',
    'Gi1/0/2   uplink-b    notconnect   trunk     full    1000 1000BaseTX',
    'Gi1/0/3   ap-floor2   connected    20        full    1000 1000BaseTX',
    'Te1/1/1   core-spine  connected    trunk     full   10000 10GBase-SR',
    '',
    { p: 'edge-sw1#', c: 'show ip interface brief | exclude unassigned' },
    'Interface              IP-Address      OK? Status    Protocol',
    'GigabitEthernet1/0/1   192.0.2.10      YES up        up',
    'GigabitEthernet1/0/2   198.51.100.42   YES down      down',
    'Vlan10                 203.0.113.1     YES up        up',
    '',
    '%LINK-3-UPDOWN: Interface GigabitEthernet1/0/2, changed state to down',
    '%LINEPROTO-5-UPDOWN: Line protocol on Gi1/0/2, changed state to down',
  ];

  const PROFILES = {
    cisco: {
      title: 'ptty ssh edge-sw1.example.net',
      lines: [
        { p: 'edge-sw1#', c: 'show ip interface brief' },
        'Interface              IP-Address      OK? Status    Protocol',
        'GigabitEthernet1/0/1   192.0.2.10      YES up        up',
        'GigabitEthernet1/0/2   198.51.100.42   YES down      down',
        'Vlan10                 203.0.113.1     YES up        up',
        '',
        '%LINK-3-UPDOWN: Interface Gi1/0/2, changed state to down',
      ],
    },
    juniper: {
      title: 'ptty ssh core-rtr.example.net',
      lines: [
        { p: 'netops@core-rtr>', c: ' show interfaces terse' },
        'Interface               Admin Link Proto    Local',
        'ge-0/0/0                up    up',
        'ge-0/0/0.0              up    up   inet     192.0.2.2/31',
        'ge-0/0/1                up    down',
        'ae0.0                   up    up   inet     203.0.113.9/30',
        'xe-0/1/0.0              up    up   inet     198.51.100.1/30',
      ],
    },
    fortinet: {
      title: 'ptty ssh fw-edge.example.net',
      lines: [
        { p: 'fw-edge #', c: 'get system interface physical' },
        '== [ port1 ]',
        'name: port1   mode: static   ip: 192.0.2.1 255.255.255.0     status: up',
        '== [ port2 ]',
        'name: port2   mode: dhcp     ip: 0.0.0.0 0.0.0.0             status: down',
        '== [ port3 ]',
        'name: port3   mode: static   ip: 198.51.100.1 255.255.255.0  status: up',
      ],
    },
    arista: {
      title: 'ptty ssh spine1.example.net',
      lines: [
        { p: 'spine1>', c: 'show ip interface brief' },
        'Interface       IP Address         Status     Protocol   MTU',
        'Ethernet1       198.51.100.0/31    up         up         1500',
        'Ethernet2       198.51.100.2/31    down       down       1500',
        'Ethernet3       198.51.100.4/31    up         up         9214',
        'Management1     192.0.2.50/24      up         up         1500',
      ],
    },
    'linux-unix': {
      title: 'ptty /bin/zsh',
      lines: [
        { p: 'ops@workstation%', c: ' ip -br addr show' },
        'lo               UNKNOWN        127.0.0.1/8',
        'eth0             UP             192.0.2.15/24',
        'eth1             DOWN',
        'wg0              UNKNOWN        203.0.113.8/24',
        { p: 'ops@workstation%', c: ' systemctl is-active nginx' },
        'active',
      ],
    },
  };

  /* ---- 1. animated hero terminal ---- */
  async function runHero() {
    const body = document.querySelector('[data-terminal]');
    if (!body) return;

    if (reduceMotion) {
      body.innerHTML = render(HERO, lineHTML);
      return;
    }

    while (true) {
      body.innerHTML = '';
      for (const line of HERO) {
        const el = document.createElement('span');
        el.className = 'tline';
        body.appendChild(el);

        if (typeof line === 'object' && line.c) {
          el.innerHTML = `<span class="hl-host">${esc(line.p)}</span><span class="t-cmd"></span><span class="cursor">.</span>`;
          const cmd = el.querySelector('.t-cmd');
          for (const ch of line.c) {
            cmd.append(ch);
            await sleep(rand(26, 64));
          }
          el.querySelector('.cursor').remove();
          await sleep(440);
        } else {
          el.innerHTML = blank(highlight(line));
          await sleep(line === '' ? 70 : 150);
        }
      }
      const tail = document.createElement('span');
      tail.className = 'tline';
      tail.innerHTML = '<span class="hl-host">edge-sw1#</span><span class="cursor">.</span>';
      body.appendChild(tail);
      await sleep(5200);
    }
  }

  /* ---- 2. raw / highlighted compare slider ---- */
  function setComparePosition(root, value, source = 'program') {
    const normalized = Math.max(0, Math.min(100, Number(value)));
    const highlightedPercentage = 100 - normalized;
    const controls = root.closest('.instrument-shell') || root;
    const range = controls.querySelector('[data-compare-range]');
    const output = controls.querySelector('[data-compare-output]');
    root.style.setProperty('--compare-position', `${normalized}%`);
    range.value = String(normalized);
    range.setAttribute('aria-valuetext', `${highlightedPercentage}% highlighted`);
    output.value = `${highlightedPercentage}% highlighted`;
    controls.querySelectorAll('[data-compare-mode]').forEach((button) => {
      const pressed = (button.dataset.compareMode === 'raw' && normalized === 100)
        || (button.dataset.compareMode === 'highlighted' && normalized === 0);
      button.setAttribute('aria-pressed', String(pressed));
    });
    root.dataset.compareSource = source;
  }

  function initCompare() {
    const root = document.querySelector('[data-compare]');
    if (!root) return;
    const controls = root.closest('.instrument-shell') || root;
    const raw = root.querySelector('[data-compare-raw]');
    const highlighted = root.querySelector('[data-compare-hl]');
    const range = controls.querySelector('[data-compare-range]');
    const buttons = controls.querySelectorAll('[data-compare-mode]');
    const rangeControl = controls.querySelector('.compare-control');
    const mobileControls = controls.querySelector('.compare-mobile-controls');
    raw.innerHTML = render(COMPARE, rawHTML);
    highlighted.innerHTML = render(COMPARE, lineHTML);
    setComparePosition(root, range.value);

    let syncingScroll = false;
    const syncScroll = (source, target) => {
      if (syncingScroll || target.scrollLeft === source.scrollLeft) return;
      syncingScroll = true;
      target.scrollLeft = source.scrollLeft;
      syncingScroll = false;
    };
    raw.addEventListener('scroll', () => syncScroll(raw, highlighted), { passive: true });
    highlighted.addEventListener('scroll', () => syncScroll(highlighted, raw), { passive: true });

    range.addEventListener('input', () => setComparePosition(root, range.value, 'user'));
    buttons.forEach((button) => {
      button.addEventListener('click', () => {
        const position = button.dataset.compareMode === 'raw' ? 100 : 0;
        setComparePosition(root, position, 'user');
      });
    });

    if (!reduceMotion && 'IntersectionObserver' in window) {
      const observer = new IntersectionObserver((entries) => {
        if (root.dataset.compareSource === 'user') return;
        const visible = entries.filter((entry) => entry.isIntersecting)
          .sort((a, b) => b.intersectionRatio - a.intersectionRatio)[0];
        if (visible) setComparePosition(root, visible.target.dataset.position);
      }, { rootMargin: '-28% 0px -42%', threshold: [0.2, 0.5, 0.8] });
      document.querySelectorAll('[data-compare-step]').forEach((step) => observer.observe(step));
    }

    rangeControl.removeAttribute('hidden');
    mobileControls.removeAttribute('hidden');
  }

  /* ---- 3. interactive profile tabs ---- */
  function initProfiles() {
    const root = document.querySelector('[data-profiles]');
    if (!root) return;
    const tablist = root.querySelector('.profile-tabs');
    const tabs = [...root.querySelectorAll('[data-profile-tab]')];
    const panel = root.querySelector('[data-profile-panel]');
    const body = panel.querySelector('[data-profile-body]');
    const title = panel.querySelector('[data-profile-title]');

    const selectProfile = (tab, focus = false) => {
      const data = PROFILES[tab.dataset.profileTab];
      if (!data) return;
      tabs.forEach((item) => {
        const selected = item === tab;
        item.setAttribute('aria-selected', String(selected));
        item.tabIndex = selected ? 0 : -1;
      });
      panel.setAttribute('aria-labelledby', tab.id);
      title.textContent = data.title;
      body.innerHTML = render(data.lines, lineHTML);
      if (focus) tab.focus();
    };

    tabs.forEach((tab, index) => {
      tab.addEventListener('click', () => selectProfile(tab));
      tab.addEventListener('keydown', (event) => {
        const keys = ['ArrowLeft', 'ArrowRight', 'Home', 'End'];
        if (!keys.includes(event.key)) return;
        event.preventDefault();
        let nextIndex = index;
        if (event.key === 'ArrowLeft') nextIndex = (index - 1 + tabs.length) % tabs.length;
        if (event.key === 'ArrowRight') nextIndex = (index + 1) % tabs.length;
        if (event.key === 'Home') nextIndex = 0;
        if (event.key === 'End') nextIndex = tabs.length - 1;
        selectProfile(tabs[nextIndex], true);
      });
    });

    const selected = tabs.find((tab) => tab.getAttribute('aria-selected') === 'true') || tabs[0];
    selectProfile(selected);
    tablist.removeAttribute('hidden');
  }

  /* ---- mobile navigation ---- */
  function initMobileMenu() {
    const header = document.querySelector('[data-site-header]');
    const trigger = header?.querySelector('[data-menu-trigger]');
    const nav = header?.querySelector('[data-site-nav]');
    const label = trigger?.querySelector('.sr-only');
    const links = nav ? [...nav.querySelectorAll('a')] : [];
    if (!trigger || !nav || !label || !links.length) return;

    const close = (restoreFocus = false) => {
      trigger.setAttribute('aria-expanded', 'false');
      label.textContent = 'Open navigation';
      document.body.classList.remove('menu-open');
      if (restoreFocus) trigger.focus();
    };
    const open = () => {
      trigger.setAttribute('aria-expanded', 'true');
      label.textContent = 'Close navigation';
      document.body.classList.add('menu-open');
      links[0].focus();
    };

    trigger.addEventListener('click', () => {
      if (trigger.getAttribute('aria-expanded') === 'true') close(true);
      else open();
    });
    document.addEventListener('keydown', (event) => {
      const openNow = trigger.getAttribute('aria-expanded') === 'true';
      if (event.key === 'Escape' && openNow) {
        close(true);
        return;
      }
      if (event.key !== 'Tab' || !openNow) return;
      const focusable = [trigger, ...links];
      const currentIndex = focusable.indexOf(document.activeElement);
      if (event.shiftKey && currentIndex <= 0) {
        event.preventDefault();
        focusable[focusable.length - 1].focus();
      } else if (!event.shiftKey && currentIndex === focusable.length - 1) {
        event.preventDefault();
        focusable[0].focus();
      }
    });
    links.forEach((link) => {
      link.addEventListener('click', () => {
        const href = link.getAttribute('href');
        const destination = href?.startsWith('#') ? document.querySelector(href) : null;
        if (destination) {
          const hadTabindex = destination.hasAttribute('tabindex');
          if (!hadTabindex) destination.setAttribute('tabindex', '-1');
          destination.focus({ preventScroll: true });
          if (!hadTabindex) {
            destination.addEventListener('blur', () => destination.removeAttribute('tabindex'), { once: true });
          }
        }
        close();
      });
    });
    const desktopQuery = window.matchMedia('(min-width: 768px)');
    const handleViewportChange = (event) => {
      if (event.matches) close();
    };
    if ('addEventListener' in desktopQuery) desktopQuery.addEventListener('change', handleViewportChange);
    else desktopQuery.addListener(handleViewportChange);

    close();
    document.body.classList.add('mobile-nav-ready');
    trigger.removeAttribute('hidden');
  }

  /* ---- installation method and copy feedback ---- */
  const INSTALL_METHODS = {
    homebrew: 'brew install inxbit/tap/prismtty',
    cargo: 'cargo install prismtty',
  };

  function initInstallMethods() {
    const root = document.querySelector('.install-section');
    const group = root?.querySelector('.install-methods');
    const buttons = root ? [...root.querySelectorAll('[data-install-method]')] : [];
    const command = root?.querySelector('[data-install-command]');
    if (!root || !group || !command || !buttons.length) return;
    if (buttons.some((button) => !INSTALL_METHODS[button.dataset.installMethod])) return;

    const selectMethod = (button) => {
      buttons.forEach((item) => {
        item.setAttribute('aria-pressed', String(item === button));
      });
      command.textContent = INSTALL_METHODS[button.dataset.installMethod];
    };
    buttons.forEach((button) => {
      button.addEventListener('click', () => {
        selectMethod(button);
        root.dispatchEvent(new Event('installmethodchange'));
      });
    });

    const selected = buttons.find((button) => button.getAttribute('aria-pressed') === 'true') || buttons[0];
    selectMethod(selected);
    group.removeAttribute('hidden');
  }

  function initCopy() {
    const root = document.querySelector('.install-section');
    const button = root?.querySelector('[data-copy-command]');
    const command = root?.querySelector('[data-install-command]');
    const status = root?.querySelector('[data-copy-status]');
    const methodButtons = root ? [...root.querySelectorAll('[data-install-method]')] : [];
    if (!root || !button || !command || !status) return;

    let resetTimer;
    let copySequence = 0;
    const resetButton = () => {
      window.clearTimeout(resetTimer);
      button.textContent = 'Copy command';
      button.removeAttribute('data-copy-state');
    };
    const setCopyPending = (pending) => {
      button.disabled = pending;
      methodButtons.forEach((methodButton) => {
        methodButton.disabled = pending;
      });
      if (pending) button.setAttribute('aria-busy', 'true');
      else button.removeAttribute('aria-busy');
    };
    const selectCommand = () => {
      command.focus();
      const selection = window.getSelection();
      if (!selection) return;
      const range = document.createRange();
      range.selectNodeContents(command);
      selection.removeAllRanges();
      selection.addRange(range);
    };

    button.addEventListener('click', async () => {
      const text = command.textContent.trim();
      const operation = ++copySequence;
      resetButton();
      setCopyPending(true);
      status.textContent = 'Copying command';
      button.textContent = 'Copying';
      try {
        if (!Object.values(INSTALL_METHODS).includes(text)) throw new Error('Unknown install command');
        await navigator.clipboard.writeText(text);
        if (operation !== copySequence || command.textContent.trim() !== text) return;
        status.textContent = 'Command copied';
        button.textContent = 'Copied';
        button.dataset.copyState = 'success';
      } catch {
        if (operation !== copySequence || command.textContent.trim() !== text) return;
        status.textContent = 'Select the command and copy it manually';
        button.textContent = 'Select command';
        button.dataset.copyState = 'failure';
        selectCommand();
      } finally {
        if (operation === copySequence) setCopyPending(false);
      }
      if (operation !== copySequence) return;
      window.clearTimeout(resetTimer);
      resetTimer = window.setTimeout(resetButton, 1600);
    });
    root.addEventListener('installmethodchange', () => {
      copySequence += 1;
      setCopyPending(false);
      status.textContent = '';
      resetButton();
    });

    button.removeAttribute('hidden');
  }

  /* ---- scroll reveal ---- */
  function initReveal() {
    const targets = document.querySelectorAll('.section, .command-band');
    if (reduceMotion || !('IntersectionObserver' in window)) return;
    targets.forEach((el) => el.classList.add('reveal'));
    const io = new IntersectionObserver(
      (entries, obs) => {
        entries.forEach((e) => {
          if (e.isIntersecting) {
            e.target.classList.add('is-in');
            obs.unobserve(e.target);
          }
        });
      },
      { rootMargin: '0px 0px -12% 0px', threshold: 0.08 }
    );
    targets.forEach((el) => io.observe(el));
  }

  /* ---- active nav state ---- */
  function initNav() {
    const nav = document.querySelector('[data-site-nav]');
    const links = nav ? [...nav.querySelectorAll('a[href^="#"]')] : [];
    const sections = links
      .map((l) => document.querySelector(l.getAttribute('href')))
      .filter(Boolean);
    if (!sections.length || !('IntersectionObserver' in window)) return;

    const io = new IntersectionObserver(
      (entries) => {
        const visible = entries
          .filter((e) => e.isIntersecting)
          .sort((a, b) => b.intersectionRatio - a.intersectionRatio)[0];
        if (!visible) return;
        links.forEach((link) => {
          const active = link.getAttribute('href') === `#${visible.target.id}`;
          link.classList.toggle('is-active', active);
          if (active) link.setAttribute('aria-current', 'location');
          else link.removeAttribute('aria-current');
        });
      },
      { rootMargin: '-18% 0px -62% 0px', threshold: [0.1, 0.25, 0.5] }
    );
    sections.forEach((s) => io.observe(s));
  }

  initMobileMenu();
  initCompare();
  initProfiles();
  initInstallMethods();
  initCopy();
  initReveal();
  initNav();
  runHero();
})();
