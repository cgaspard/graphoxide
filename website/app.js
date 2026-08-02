const header = document.querySelector('[data-header]');
const navToggle = document.querySelector('[data-nav-toggle]');
const nav = document.querySelector('[data-nav]');

const setNavOpen = (open) => {
  navToggle?.setAttribute('aria-expanded', String(open));
  nav?.classList.toggle('is-open', open);
  const label = navToggle?.querySelector('.sr-only');
  if (label) label.textContent = open ? 'Close navigation' : 'Open navigation';
};

navToggle?.addEventListener('click', () => {
  setNavOpen(navToggle.getAttribute('aria-expanded') !== 'true');
});

nav?.querySelectorAll('a').forEach((link) => link.addEventListener('click', () => setNavOpen(false)));
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape') setNavOpen(false);
});

const syncHeader = () => header?.classList.toggle('is-scrolled', window.scrollY > 8);
syncHeader();
window.addEventListener('scroll', syncHeader, { passive: true });

const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
const reveals = document.querySelectorAll('.reveal');
if (reducedMotion || !('IntersectionObserver' in window)) {
  reveals.forEach((element) => element.classList.add('is-visible'));
} else {
  const observer = new IntersectionObserver((entries) => {
    entries.forEach((entry) => {
      if (!entry.isIntersecting) return;
      entry.target.classList.add('is-visible');
      observer.unobserve(entry.target);
    });
  }, { rootMargin: '0px 0px -8% 0px', threshold: 0.08 });
  reveals.forEach((element) => observer.observe(element));
}

const queries = {
  auth: {
    command: 'graphoxide query "how does authentication work?"',
    head: '7 relevant nodes · 11 relationships',
    rows: [
      ['AuthMiddleware', 'intercepts requests'],
      ['verify_token', 'validates JWT signature'],
      ['UserRepository', 'loads identity and roles'],
    ],
  },
  impact: {
    command: 'graphoxide affected User --depth 3',
    head: '14 affected nodes · depth 3',
    rows: [
      ['AuthService', 'reads User roles'],
      ['SessionStore', 'serializes User ID'],
      ['AccountRoutes', 'returns User payload'],
    ],
  },
  path: {
    command: 'graphoxide path ApiRouter Database',
    head: '4 hops · 5 nodes',
    rows: [
      ['ApiRouter', 'calls CheckoutService'],
      ['CheckoutService', 'uses OrderRepository'],
      ['OrderRepository', 'queries Database'],
    ],
  },
};

const queryDemo = document.querySelector('[data-query-demo]');
queryDemo?.querySelectorAll('[data-query]').forEach((button) => {
  button.addEventListener('click', () => {
    const value = queries[button.dataset.query];
    if (!value) return;
    queryDemo.querySelectorAll('[data-query]').forEach((item) => item.classList.toggle('is-active', item === button));
    queryDemo.querySelector('[data-query-command]').textContent = value.command;
    const response = queryDemo.querySelector('[data-query-response]');
    response.querySelector('.response-head').innerHTML = `<span></span> ${value.head}`;
    response.querySelector('ol').replaceChildren(...value.rows.map(([name, relation]) => {
      const row = document.createElement('li');
      const label = document.createElement('b');
      const detail = document.createElement('span');
      label.textContent = name;
      detail.textContent = relation;
      row.append(label, detail);
      return row;
    }));
  });
});

const installs = {
  cargo: ['cargo install --git https://github.com/cgaspard/graphoxide graphoxide-cli', 'Builds the CLI from the current main branch and installs it on your Cargo PATH.'],
  release: ['gh release download --repo cgaspard/graphoxide --pattern "graphoxide-*"', 'Downloads release artifacts; choose the archive for your platform and put graphoxide on PATH.'],
  source: ['cargo install --path crates/graphoxide-cli', 'Builds and installs from an existing local Graphoxide checkout.'],
};

document.querySelectorAll('[data-install]').forEach((button) => {
  button.addEventListener('click', () => {
    const value = installs[button.dataset.install];
    if (!value) return;
    document.querySelectorAll('[data-install]').forEach((item) => item.classList.toggle('is-active', item === button));
    document.querySelector('[data-install-command]').textContent = value[0];
    document.querySelector('[data-install-note]').textContent = value[1];
  });
});

document.querySelectorAll('[data-copy-target]').forEach((button) => {
  button.addEventListener('click', async () => {
    const target = document.querySelector(button.dataset.copyTarget);
    if (!target) return;
    const previous = button.textContent;
    try {
      await navigator.clipboard.writeText(target.innerText || target.textContent);
      button.textContent = 'Copied';
    } catch {
      button.textContent = 'Select to copy';
    }
    window.setTimeout(() => { button.textContent = previous; }, 1600);
  });
});

document.querySelectorAll('[data-year]').forEach((element) => {
  element.textContent = String(new Date().getFullYear());
});
