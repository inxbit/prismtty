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

    // Idle the typing loop while the tab is hidden or the hero is scrolled away.
    let onScreen = true;
    if ('IntersectionObserver' in window) {
      onScreen = false;
      new IntersectionObserver((entries) => {
        onScreen = entries[0].isIntersecting;
      }).observe(body);
    }
    const gate = async () => {
      while (document.hidden || !onScreen) await sleep(300);
    };

    while (true) {
      body.innerHTML = '';
      for (const line of HERO) {
        await gate();
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
  function initCompare() {
    const wrap = document.querySelector('[data-compare]');
    if (!wrap) return;
    const raw = wrap.querySelector('[data-compare-raw]');
    const hl = wrap.querySelector('[data-compare-hl]');
    const range = wrap.querySelector('[data-compare-range]');

    raw.innerHTML = render(COMPARE, rawHTML);
    hl.innerHTML = render(COMPARE, lineHTML);

    const setPos = (v) => wrap.style.setProperty('--pos', `${v}%`);
    setPos(range.value);
    range.addEventListener('input', () => setPos(range.value));
  }

  /* ---- 3. interactive profile tabs ---- */
  function initProfiles() {
    const root = document.querySelector('[data-profiles]');
    if (!root) return;
    const tabs = [...root.querySelectorAll('.profile-tab')];
    const body = root.querySelector('[data-profile-body]');
    const title = root.querySelector('[data-profile-title]');
    const badge = root.querySelector('[data-profile-badge]');

    const show = (key) => {
      const data = PROFILES[key];
      if (!data) return;
      const paint = () => {
        body.innerHTML = render(data.lines, lineHTML);
        title.textContent = data.title;
        if (badge) badge.textContent = key;
        body.classList.remove('is-swapping');
      };
      if (reduceMotion) { paint(); return; }
      body.classList.add('is-swapping');
      setTimeout(paint, 150);
    };

    const select = (tab) => {
      tabs.forEach((t) => {
        t.setAttribute('aria-selected', String(t === tab));
        t.tabIndex = t === tab ? 0 : -1;
      });
      show(tab.dataset.profile);
    };

    tabs.forEach((tab, i) => {
      tab.addEventListener('click', () => select(tab));
      tab.addEventListener('keydown', (e) => {
        const to = {
          ArrowRight: i + 1,
          ArrowDown: i + 1,
          ArrowLeft: i - 1,
          ArrowUp: i - 1,
          Home: 0,
          End: tabs.length - 1,
        }[e.key];
        if (to === undefined) return;
        e.preventDefault();
        const next = tabs[(to + tabs.length) % tabs.length];
        next.focus();
        select(next);
      });
    });

    const initial =
      tabs.find((t) => t.getAttribute('aria-selected') === 'true') || tabs[0];
    if (initial) select(initial);
  }

  /* ---- copy buttons ---- */
  function initCopy() {
    document.querySelectorAll('[data-copy]').forEach((btn) => {
      const original = btn.textContent;
      btn.addEventListener('click', async () => {
        try {
          await navigator.clipboard.writeText(btn.dataset.copy);
          btn.textContent = 'Copied ✓';
        } catch {
          btn.textContent = 'Select all';
        }
        btn.classList.add('copied');
        setTimeout(() => {
          btn.textContent = original;
          btn.classList.remove('copied');
        }, 1600);
      });
    });
  }

  /* ---- active nav state ---- */
  function initNav() {
    const links = [...document.querySelectorAll('.site-nav a[href^="#"]')];
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
        links.forEach((l) =>
          l.classList.toggle('is-active', l.getAttribute('href') === `#${visible.target.id}`)
        );
      },
      { rootMargin: '-18% 0px -62% 0px', threshold: [0.1, 0.25, 0.5] }
    );
    sections.forEach((s) => io.observe(s));
  }

  initCompare();
  initProfiles();
  initCopy();
  initNav();
  runHero();
})();
