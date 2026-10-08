// prismtty.com: copy buttons and the prism at the hero seam.
(() => {
  const root = document.documentElement;
  const still = window.matchMedia('(prefers-reduced-motion: reduce)');

  // Copy buttons: the command lives in data-copy. The button keeps its label;
  // the icon turns into a check and the status line after it says what
  // happened. If the clipboard is refused (no API, insecure context, denied
  // permission), the command is selected so Ctrl+C or Cmd+C finishes the job.
  for (const button of document.querySelectorAll('[data-copy]')) {
    const code = button.previousElementSibling;
    const status = button.nextElementSibling;
    let reset = 0;
    button.addEventListener('click', async () => {
      clearTimeout(reset);
      try {
        await navigator.clipboard.writeText(button.dataset.copy);
        button.dataset.copied = '';
        status.textContent = 'Copied';
        reset = setTimeout(() => {
          delete button.dataset.copied;
          status.textContent = '';
        }, 1600);
      } catch {
        delete button.dataset.copied;
        // Select the command but not its $ prompt.
        const range = document.createRange();
        range.selectNodeContents(code);
        const prompt = code.querySelector('.install-prompt');
        if (prompt) range.setStartAfter(prompt);
        const selection = window.getSelection();
        selection.removeAllRanges();
        selection.addRange(range);
        status.textContent = 'Press Ctrl+C or Cmd+C to copy';
      }
    });
  }

  // Panes that scroll sideways get .scrolls (and .at-end once read to the
  // end) so CSS can fade the cut edge; touch devices show no scrollbar.
  const panes = [...document.querySelectorAll('.out, .out-block, .install-cmd, .install-line')];
  const markOverflow = () => {
    for (const pane of panes) {
      const scrolls = pane.scrollWidth - pane.clientWidth > 1;
      pane.classList.toggle('scrolls', scrolls);
      pane.classList.toggle('at-end', scrolls && pane.scrollLeft + pane.clientWidth >= pane.scrollWidth - 2);
    }
  };
  for (const pane of panes) pane.addEventListener('scroll', markOverflow, { passive: true });
  window.addEventListener('resize', markOverflow);
  markOverflow();
  if (document.fonts) document.fonts.ready.then(markOverflow);

  // Profile tabs: roving tabindex, arrow keys, Home and End.
  for (const group of document.querySelectorAll('[data-tabs]')) {
    const tabs = [...group.querySelectorAll('[role="tab"]')];
    const name = group.querySelector('[data-profile-name]');
    const select = (tab, focus) => {
      for (const other of tabs) {
        const on = other === tab;
        other.setAttribute('aria-selected', String(on));
        other.tabIndex = on ? 0 : -1;
        document.getElementById(other.getAttribute('aria-controls')).hidden = !on;
      }
      if (name) name.textContent = tab.textContent;
      markOverflow();
      if (focus) tab.focus();
    };
    tabs.forEach((tab, index) => {
      tab.addEventListener('click', () => select(tab, false));
      tab.addEventListener('keydown', (event) => {
        const last = tabs.length - 1;
        const next = { ArrowRight: index + 1, ArrowLeft: index - 1, Home: 0, End: last }[event.key];
        if (next === undefined) return;
        event.preventDefault();
        select(tabs[(next + tabs.length) % tabs.length], true);
      });
    });
    select(tabs.find((tab) => tab.getAttribute('aria-selected') === 'true') || tabs[0], false);
  }

  // The prism at the seam. With motion allowed, the footage plays once
  // until the spectrum is wide and eases to a stop on that frame; the light
  // layer (a steady flow along the beam, dust in the light) fades in as it
  // settles. Without motion the still plate stays. When the browser asks to
  // save data the footage is skipped: the plate stays and the light comes on.
  const video = document.querySelector('.seam-video');
  const EASE = 1.4; // seconds of footage over which playback slows to a stop
  let settled = Boolean(navigator.connection?.saveData); // nothing left to play
  let easing = 0; // animation frame of the ease
  let stall = 0; // timer that gives up on footage that stopped arriving

  // The intro is over: hold the frame it stopped on and bring in the light.
  function settle() {
    settled = true;
    clearTimeout(stall);
    cancelAnimationFrame(easing);
    easing = 0;
    video.pause();
    root.classList.add('prism-live');
  }

  // The footage broke: drop it and fall back to the still plate.
  function giveUp() {
    settle();
    video.hidden = true;
    root.classList.remove('prism-playing');
  }

  // One step per frame near the end. playbackRate = sqrt(left / EASE) is a
  // quadratic ease-out that lands on the last frame about 2.4 s after the
  // window opens. 0.0625 is the lowest rate Chromium accepts; the rate
  // stays below 1 throughout (see the WebKit note in start).
  function ease() {
    easing = 0;
    if (settled || video.paused) return;
    const left = video.duration - video.currentTime;
    if (video.ended || left <= 0.03) {
      settle();
      return;
    }
    if (left < EASE) {
      const k = left / EASE;
      video.playbackRate = Math.max(0.0625, Math.sqrt(k));
      if (k < 0.7) root.classList.add('prism-live');
    }
    easing = requestAnimationFrame(ease);
  }

  // No new footage for 4 s while it should be playing: give up on it.
  function watch(event) {
    if (event.type === 'waiting' && !video.currentTime) return; // first load
    clearTimeout(stall);
    const at = video.currentTime;
    stall = setTimeout(() => {
      if (!settled && !video.paused && video.currentTime === at) giveUp();
    }, 4000);
  }

  if (video) {
    video.addEventListener('playing', () => {
      if (settled) {
        video.pause();
        return;
      }
      clearTimeout(stall);
      video.hidden = false;
      root.classList.add('prism-playing');
    });
    // timeupdate (about 4 Hz) only opens the window; the frames do the ease.
    video.addEventListener('timeupdate', () => {
      const left = video.duration - video.currentTime;
      if (!easing && !settled && !video.paused && left < EASE + 0.5) {
        easing = requestAnimationFrame(ease);
      }
    });
    video.addEventListener('ended', settle);
    video.addEventListener('error', giveUp);
    video.addEventListener('abort', giveUp);
    video.addEventListener('stalled', watch);
    video.addEventListener('waiting', watch);
  }

  // Dust in the beam, drawn only while the hero is on screen and the light
  // layer has a box (it is hidden on narrower screens).
  const canvas = document.querySelector('.dust');
  let motes = [];
  let frame = 0;
  let visible = false;
  let drawable = false;

  function sizeCanvas() {
    const box = canvas.getBoundingClientRect();
    const scale = window.devicePixelRatio || 1;
    drawable = box.width > 0 && box.height > 0;
    canvas.width = Math.round(box.width * scale);
    canvas.height = Math.round(box.height * scale);
    // Deterministic placement: the same motes on every load.
    motes = Array.from({ length: 34 }, (_, i) => ({
      x: ((i * 0.6180339887) % 1) * canvas.width,
      y: (0.08 + 0.84 * ((i * 0.3819660113) % 1)) * canvas.height,
      r: (0.5 + ((i * 7) % 5) * 0.22) * scale,
      v: (0.1 + ((i * 3) % 7) * 0.025) * scale,
    }));
  }

  // How lit a point is: dust only shows where it crosses the beam (left of
  // the prism) or the spectrum fan (right of it). Fractions of the canvas.
  function light(x, y) {
    const beamY = 0.405;
    if (x < 0.47) return Math.max(0, 1 - Math.abs(y - beamY) / 0.045);
    if (x < 0.68) return 0.15;
    const spread = 0.05 + (x - 0.68) * 0.9;
    const centre = beamY + (x - 0.68) * 0.25;
    return Math.max(0, 1 - Math.abs(y - centre) / spread) * 0.9;
  }

  function draw() {
    const ctx = canvas.getContext('2d');
    const w = canvas.width;
    const h = canvas.height;
    ctx.clearRect(0, 0, w, h);
    ctx.fillStyle = '#ffffff';
    for (const m of motes) {
      m.x = (m.x + m.v) % w;
      const y = m.y + Math.sin((m.x + m.y) / 60) * 3;
      const fade = Math.min(1, Math.min(m.x, w - m.x) / (w * 0.08));
      const lit = 0.06 + 0.8 * light(m.x / w, y / h);
      ctx.globalAlpha = lit * fade;
      ctx.beginPath();
      ctx.arc(m.x, y, m.r, 0, Math.PI * 2);
      ctx.fill();
    }
    frame = visible && drawable ? requestAnimationFrame(draw) : 0;
  }

  // Run whatever should be running: motion allowed, tab visible, hero on
  // screen (the callers check that).
  function start() {
    if (still.matches || document.visibilityState !== 'visible') return;
    if (canvas && drawable && !frame) frame = requestAnimationFrame(draw);
    if (!video) return;
    if (settled) {
      root.classList.add('prism-live');
      return;
    }
    if (!video.src) {
      video.preload = 'auto';
      video.src = 'media/prism-intro.mp4';
      // WebKit (seen in Playwright's build) replays the clip from the start
      // when the rate moves off or back to 1 during playback. Leaving 1
      // here, before the first frame, and never returning to it keeps the
      // ease from rewinding the footage.
      video.playbackRate = 0.999;
    }
    video.play().catch((error) => {
      // stop() pausing a pending play() is not a failure; a refused
      // autoplay or an unplayable source is.
      if (error.name !== 'AbortError') giveUp();
    });
  }

  function stop() {
    if (frame) cancelAnimationFrame(frame);
    frame = 0;
    if (easing) cancelAnimationFrame(easing);
    easing = 0;
    clearTimeout(stall);
    if (video && !video.paused) video.pause();
  }

  if (canvas) {
    sizeCanvas();
    window.addEventListener('resize', () => {
      sizeCanvas();
      if (visible) start();
    });
  }

  const hero = document.querySelector('.hero');
  if (hero && 'IntersectionObserver' in window) {
    new IntersectionObserver(([entry]) => {
      visible = entry.isIntersecting;
      if (visible) start();
      else stop();
    }).observe(hero);
  }

  // A background tab holds the intro until the visitor is looking.
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState !== 'visible') stop();
    else if (visible) start();
  });

  // Reduced motion switched on: back to the still plate. Off: carry on.
  still.addEventListener('change', () => {
    if (still.matches) {
      stop();
      root.classList.remove('prism-live', 'prism-playing');
      if (video) video.hidden = true;
    } else if (visible) {
      start();
    }
  });
})();
