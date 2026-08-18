/* Общая полоса навигации. Модули не знают друг о друге — только про манифест,
   поэтому новый модуль появляется во всех страницах сразу после правки modules.js. */
import { MODULES, HUB, ROOT } from './modules.js';

export function mountNav(activeId) {
  const nav = document.createElement('nav');
  nav.className = 'eco-nav';

  const home = document.createElement('a');
  home.className = 'home';
  home.href = ROOT + HUB.path;
  home.textContent = '\u25c0 ' + HUB.name;
  nav.append(home);

  for (const m of MODULES) {
    const a = document.createElement('a');
    a.href = ROOT + m.path;
    a.textContent = m.short;
    a.title = m.desc;
    if (m.id === activeId) a.setAttribute('aria-current', 'page');
    nav.append(a);
  }

  const spacer = document.createElement('span');
  spacer.className = 'spacer';
  const tag = document.createElement('span');
  tag.className = 'tag on';
  tag.textContent = 'HOUSE ECOSYSTEM';
  nav.append(spacer, tag);

  document.body.prepend(nav);
}

export function mountFooter(text) {
  const f = document.createElement('footer');
  f.className = 'eco-foot';
  f.textContent = text;
  document.body.append(f);
}
