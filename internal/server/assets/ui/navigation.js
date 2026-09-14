// All three navigation groups use the same selection and persistence rules.
function selectView(name, { prefix, buttons, attribute, pages, fallback, storage }) {
  if (!document.getElementById(`${prefix}-${name}`)) name = fallback;
  document.querySelectorAll(buttons).forEach(button => {
    const selected = button.getAttribute(attribute) === name;
    button.classList.toggle('active', selected);
    button.setAttribute('aria-pressed', String(selected));
  });
  document.querySelectorAll(pages).forEach(page => {
    page.classList.toggle('active', page.id === `${prefix}-${name}`);
  });
  localStorage.setItem(storage, name);
  return name;
}

export function setTab(name) {
  name = { git: 'connections', mcp: 'connections', snapshots: 'environments' }[name] || name;
  name = selectView(name, {
    prefix: 'tab', buttons: '.tab-button', attribute: 'data-tab',
    pages: '.tab-page', fallback: 'jobs', storage: 'spin-tab',
  });
  document.querySelector('main.workspace')?.classList.toggle('explore', name === 'explore');
}

export function setConnection(name) {
  selectView(name, {
    prefix: 'connection', buttons: '[data-connection]', attribute: 'data-connection',
    pages: '.connection-page', fallback: 'git', storage: 'spin-connection',
  });
}

export function setWorkView(name) {
  selectView(name, {
    prefix: 'work', buttons: '[data-work-view]', attribute: 'data-work-view',
    pages: '.work-page', fallback: 'jobs', storage: 'spin-work-view',
  });
}
