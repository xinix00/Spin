/* Tactile adapter for @xinix00/markdown 1.6.1. No application dependencies. */
(function (global) {
  'use strict';
  const instances = new Map();
  let sequence = 0;
  const labels = {
    bold: 'Vet', italic: 'Cursief', strike: 'Doorhalen',
    h1: 'Kop 1', h2: 'Kop 2', h3: 'Kop 3', bullet: 'Opsomming',
    numbered: 'Genummerde lijst', quote: 'Citaat', code: 'Code',
    table: 'Tabel', paragraph: 'Alinea', placeholder: 'Schrijf hier…'
  };

  const icons = { bold: 'format_bold', italic: 'format_italic', strike: 'strikethrough_s',
    h1: 'format_h1', h2: 'format_h2', h3: 'format_h3', bullet: 'format_list_bulleted',
    numbered: 'format_list_numbered', quote: 'format_quote', code: 'code', table: 'table', paragraph: 'format_paragraph' };
  function mount(root = document) {
    if (!global.MarkdownEditor) return [];
    const areas = [...root.querySelectorAll('textarea[data-markdown]')];
    if (root.matches?.('textarea[data-markdown]')) areas.unshift(root);
    return areas.map(area => {
      if (instances.has(area)) { instances.get(area).refresh(); return instances.get(area); }
      const wrapper = document.createElement('div');
      wrapper.className = 't-editor';
      area.before(wrapper);
      wrapper.append(area);
      const editor = global.MarkdownEditor.mount(wrapper, { labels, placeholder: area.placeholder });
      const abort = new AbortController();
      const on = (node, type, fn) => node.addEventListener(type, fn, { signal: abort.signal });
      const error = document.createElement('p');
      error.id = `t-editor-error-${++sequence}`;
      error.className = 't-editor-error';
      error.hidden = true;
      error.setAttribute('aria-live', 'polite');
      wrapper.append(error);
      const fieldLabel = area.labels?.[0] || wrapper.previousElementSibling?.closest('label');
      const name = area.getAttribute('aria-label') || fieldLabel?.textContent?.trim() || area.placeholder || 'Markdown';
      wrapper.setAttribute('role', 'group');
      wrapper.setAttribute('aria-label', name);
      const description = [area.getAttribute('aria-describedby'), error.id].filter(Boolean).join(' ');

      function decorate() {
        wrapper.setAttribute('aria-disabled', String(area.disabled));
        wrapper.querySelectorAll('.md-input').forEach((input, index) => {
          input.setAttribute('aria-label', `${name} · blok ${index + 1}`);
          input.setAttribute('aria-describedby', description);
          input.disabled = area.disabled;
          input.readOnly = area.readOnly;
          input.spellcheck = area.spellcheck;
        });
        wrapper.querySelectorAll('.md-toolbar button').forEach(button => {
          button.disabled = area.disabled || area.readOnly;
          if (!button.querySelector('.t-icon') && icons[button.dataset.md]) {
            const symbol = document.createElement('span');
            symbol.className = 'material-symbols-outlined t-icon';
            symbol.setAttribute('aria-hidden', 'true');
            symbol.textContent = icons[button.dataset.md];
            button.replaceChildren(symbol);
          }
        });
      }
      const render = editor.render.bind(editor);
      editor.render = (...args) => { render(...args); decorate(); };
      function clearError() {
        error.hidden = true;
        wrapper.removeAttribute('aria-invalid');
      }
      function refresh() {
        if (editor.value() !== area.value) {
          editor.blocks = area.value.split('\n');
          editor.active = 0;
          editor.clearSelection();
          editor.render(false);
        }
        decorate();
        if (area.validity.valid) clearError();
      }
      on(area, 'input', refresh);
      on(area, 'change', refresh);
      on(area, 'invalid', event => {
        event.preventDefault();
        error.textContent = area.validationMessage;
        error.hidden = false;
        wrapper.setAttribute('aria-invalid', 'true');
        editor.focus(0, 'start');
      });
      if (fieldLabel) on(fieldLabel, 'click', event => { event.preventDefault(); editor.focus(0, 'start'); });
      if (area.form) on(area.form, 'reset', event => queueMicrotask(() => {
        if (!event.defaultPrevented) { refresh(); clearError(); }
      }));
      const attributes = new MutationObserver(decorate);
      attributes.observe(area, { attributes: true, attributeFilter: ['disabled', 'readonly', 'required'] });
      const api = {
        refresh,
        setValue(value) { area.value = String(value); refresh(); area.dispatchEvent(new Event('input', { bubbles: true })); },
        focus() { editor.focus(0, 'start'); },
        destroy() {
          abort.abort(); attributes.disconnect(); editor.destroy();
          instances.delete(area);
          area.style.removeProperty('display');
          wrapper.replaceWith(area);
        }
      };
      instances.set(area, api);
      decorate();
      return api;
    });
  }
  // Dynamic forms may remove entire steps. Release vendor document listeners.
  new MutationObserver(() => {
    instances.forEach((api, area) => { if (!area.isConnected) api.destroy(); });
  }).observe(document.documentElement, { childList: true, subtree: true });
  global.TactileMarkdown = { mount, get: area => instances.get(area) };
})(window);
