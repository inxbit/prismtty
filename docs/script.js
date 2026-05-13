const copyButtons = document.querySelectorAll('[data-copy]');

copyButtons.forEach((button) => {
  const originalText = button.textContent;

  button.addEventListener('click', async () => {
    const value = button.getAttribute('data-copy');

    try {
      await navigator.clipboard.writeText(value);
      button.textContent = 'Copied';
    } catch {
      button.textContent = 'Select';
    }

    window.setTimeout(() => {
      button.textContent = originalText;
    }, 1600);
  });
});

const header = document.querySelector('[data-site-header]');
const navLinks = [...document.querySelectorAll('.site-nav a[href^="#"]')];
const sections = navLinks
  .map((link) => document.querySelector(link.getAttribute('href')))
  .filter(Boolean);

if (header && sections.length > 0 && 'IntersectionObserver' in window) {
  const observer = new IntersectionObserver(
    (entries) => {
      const visible = entries
        .filter((entry) => entry.isIntersecting)
        .sort((a, b) => b.intersectionRatio - a.intersectionRatio)[0];

      if (!visible) {
        return;
      }

      navLinks.forEach((link) => {
        link.classList.toggle('is-active', link.getAttribute('href') === `#${visible.target.id}`);
      });
    },
    {
      rootMargin: '-20% 0px -62% 0px',
      threshold: [0.1, 0.25, 0.5],
    },
  );

  sections.forEach((section) => observer.observe(section));
}
