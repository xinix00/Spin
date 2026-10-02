/* Tactile pickers for date, time and number. The native input remains the form value;
   typing, validation, reset and FormData stay untouched. The joined glossy action opens
   a dark panel (date, time) or steps the value (number). */
(() => {
  'use strict';
  const instances = new Map(); let sequence = 0;
  const SELECTOR = 'input:is([type="date"],[type="time"],[type="number"]):not([data-native])';
  const lang = () => document.documentElement.lang || navigator.language || 'nl';
  const pad = n => String(n).padStart(2, '0');
  const iso = d => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  const parseDate = v => { const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(v || ''); return m ? new Date(+m[1], +m[2] - 1, +m[3]) : null; };
  const parseTime = v => { const m = /^(\d{2}):(\d{2})/.exec(v || ''); return m ? { h: +m[1], m: +m[2] } : null; };
  const sameDay = (a, b) => a && b && a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
  function firstWeekday() { try { const l = new Intl.Locale(lang()); const info = l.getWeekInfo?.() || l.weekInfo; if (info?.firstDay) return info.firstDay % 7; } catch {} return 1; }
  const el = (tag, className, text) => { const node = document.createElement(tag); if (className) node.className = className; if (text != null) node.textContent = text; return node; };
  function iconButton(icon, label) {
    const button = el('button', 't-action t-icon-button'); button.type = 'button'; button.setAttribute('aria-label', label);
    button.innerHTML = `<span class="material-symbols-outlined t-icon" aria-hidden="true">${icon}</span>`; return button;
  }
  function textButton(label, extra = '') { const button = el('button', `t-action ${extra}`.trim(), label); button.type = 'button'; return button; }
  function emit(input) { input.dispatchEvent(new Event('input', { bubbles: true })); input.dispatchEvent(new Event('change', { bubbles: true })); }
  function wrap(input, kind) {
    const wrapper = el('span', `t-input-action t-picker t-picker--${kind}`);
    input.before(wrapper); wrapper.append(input); return wrapper;
  }

  /* Shared popup behaviour for date and time. */
  function popupFor(input, wrapper, kind, label, icon) {
    const trigger = iconButton(icon, label);
    const popup = el('div', 't-select-popup t-picker-popup'); popup.popover = 'auto'; popup.id = `t-picker-${++sequence}`;
    popup.setAttribute('role', 'dialog'); popup.setAttribute('aria-label', input.labels?.[0]?.textContent.trim() || input.title || label);
    trigger.setAttribute('aria-haspopup', 'dialog'); trigger.setAttribute('aria-controls', popup.id); trigger.setAttribute('aria-expanded', 'false');
    wrapper.append(trigger, popup);
    const isOpen = () => popup.matches(':popover-open');
    function position() {
      const r = wrapper.getBoundingClientRect(), p = popup.getBoundingClientRect(), spaceBelow = innerHeight - r.bottom - 12, spaceAbove = r.top - 12;
      popup.style.left = `${Math.max(12, Math.min(r.left, innerWidth - p.width - 12))}px`;
      popup.style.top = `${Math.max(12, spaceBelow >= p.height || spaceBelow >= spaceAbove ? r.bottom + 6 : r.top - p.height - 6)}px`;
    }
    const api = { trigger, popup, isOpen, position, onOpen: null };
    function sync() { trigger.disabled = input.matches(':disabled') || input.readOnly; }
    function show() { sync(); if (trigger.disabled || isOpen()) return; api.onOpen?.(); popup.showPopover(); trigger.setAttribute('aria-expanded', 'true'); position(); popup.querySelector('[tabindex="0"]')?.focus({ preventScroll: true }); popup.querySelectorAll('[aria-selected="true"]').forEach(node => node.scrollIntoView({ block: 'center' })); }
    function hide(refocus) { if (isOpen()) popup.hidePopover(); trigger.setAttribute('aria-expanded', 'false'); if (refocus && input.isConnected && !input.disabled) input.focus({ preventScroll: true }); }
    const onTrigger = () => { if (isOpen()) hide(true); else show(); };
    const onInputKey = event => { if ((event.altKey && event.key === 'ArrowDown') || event.key === 'F4') { event.preventDefault(); show(); } else if (event.key === 'Escape' && isOpen()) { event.preventDefault(); hide(true); } };
    const onPopupKey = event => { if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); hide(true); } };
    const onFocusIn = event => { if (isOpen() && !wrapper.contains(event.target)) hide(false); };
    const onToggle = () => { if (!isOpen()) trigger.setAttribute('aria-expanded', 'false'); };
    const onGeometry = () => { if (isOpen()) position(); };
    trigger.addEventListener('pointerdown', event => event.preventDefault()); trigger.addEventListener('click', onTrigger);
    input.addEventListener('keydown', onInputKey); popup.addEventListener('keydown', onPopupKey); document.addEventListener('focusin', onFocusIn); popup.addEventListener('toggle', onToggle);
    window.addEventListener('resize', onGeometry); window.addEventListener('scroll', onGeometry, true);
    const observer = new MutationObserver(sync); observer.observe(input, { attributes: true, attributeFilter: ['disabled', 'readonly'] }); sync();
    Object.assign(api, { show, hide, sync, destroy() {
      hide(false); observer.disconnect(); input.removeEventListener('keydown', onInputKey); document.removeEventListener('focusin', onFocusIn);
      window.removeEventListener('resize', onGeometry); window.removeEventListener('scroll', onGeometry, true);
    } });
    return api;
  }

  /* Date: a dark calendar panel. Navigation arrows use the button recipe; days are flat cells. */
  function mountDate(input) {
    const wrapper = wrap(input, 'date'), shared = popupFor(input, wrapper, 'date', 'Kalender openen', 'calendar_month'), popup = shared.popup;
    const monthFormat = new Intl.DateTimeFormat(lang(), { month: 'long', year: 'numeric' });
    const weekdayFormat = new Intl.DateTimeFormat(lang(), { weekday: 'short' });
    const longFormat = new Intl.DateTimeFormat(lang(), { weekday: 'long', day: 'numeric', month: 'long', year: 'numeric' });
    const head = el('div', 't-picker-head'), prev = iconButton('chevron_left', 'Vorige maand'), next = iconButton('chevron_right', 'Volgende maand'), caption = el('div', 't-picker-caption');
    caption.setAttribute('aria-live', 'polite'); head.append(prev, caption, next);
    const grid = el('div', 't-picker-grid'); grid.setAttribute('role', 'grid');
    const foot = el('div', 't-picker-foot'), today = textButton('Vandaag'), clear = textButton('Wissen', 't-action--neutral');
    foot.append(today, clear); popup.append(head, grid, foot);
    let view = new Date(), focusKey = null;
    const inRange = d => { const min = parseDate(input.min), max = parseDate(input.max); return !(min && d < min) && !(max && d > max); };
    function render() {
      const y = view.getFullYear(), m = view.getMonth(), selected = parseDate(input.value), now = new Date();
      caption.textContent = monthFormat.format(view);
      const first = new Date(y, m, 1), start = new Date(y, m, 1 - ((first.getDay() - firstWeekday() + 7) % 7));
      const target = focusKey || (selected && selected.getMonth() === m && selected.getFullYear() === y ? iso(selected) : now.getMonth() === m && now.getFullYear() === y ? iso(now) : iso(first));
      grid.replaceChildren();
      const headRow = el('div', 't-picker-row'); headRow.setAttribute('role', 'row');
      for (let i = 0; i < 7; i++) { const d = new Date(start); d.setDate(start.getDate() + i); const cell = el('span', 't-picker-weekday', weekdayFormat.format(d).replace('.', '')); cell.setAttribute('role', 'columnheader'); cell.setAttribute('aria-label', new Intl.DateTimeFormat(lang(), { weekday: 'long' }).format(d)); headRow.append(cell); }
      grid.append(headRow);
      for (let r = 0; r < 6; r++) {
        const row = el('div', 't-picker-row'); row.setAttribute('role', 'row');
        for (let c = 0; c < 7; c++) {
          const d = new Date(start); d.setDate(start.getDate() + r * 7 + c);
          const cell = el('button', 't-picker-cell', String(d.getDate())); cell.type = 'button'; cell.setAttribute('role', 'gridcell'); cell.dataset.value = iso(d);
          cell.dataset.outside = String(d.getMonth() !== m); if (sameDay(d, now)) cell.dataset.today = 'true';
          cell.setAttribute('aria-selected', String(sameDay(d, selected))); cell.setAttribute('aria-label', longFormat.format(d));
          cell.disabled = !inRange(d); cell.tabIndex = iso(d) === target ? 0 : -1; row.append(cell);
        }
        grid.append(row);
      }
      clear.hidden = input.required; clear.disabled = !input.value;
    }
    function set(value) { if (input.value !== value) { input.value = value; emit(input); } shared.hide(true); }
    function move(base, days, months = 0) {
      const d = new Date(base); d.setMonth(d.getMonth() + months); d.setDate(d.getDate() + days);
      view = new Date(d.getFullYear(), d.getMonth(), 1); focusKey = iso(d); render(); grid.querySelector('[tabindex="0"]')?.focus({ preventScroll: true });
    }
    grid.addEventListener('click', event => { const cell = event.target.closest('.t-picker-cell'); if (cell && !cell.disabled) set(cell.dataset.value); });
    grid.addEventListener('keydown', event => {
      const cell = event.target.closest('.t-picker-cell'); if (!cell) return; const base = parseDate(cell.dataset.value);
      const map = { ArrowLeft: [-1], ArrowRight: [1], ArrowUp: [-7], ArrowDown: [7], PageUp: [0, event.shiftKey ? -12 : -1], PageDown: [0, event.shiftKey ? 12 : 1] };
      if (map[event.key]) { event.preventDefault(); event.stopPropagation(); move(base, ...map[event.key]); }
      else if (event.key === 'Home' || event.key === 'End') { event.preventDefault(); event.stopPropagation(); const offset = (base.getDay() - firstWeekday() + 7) % 7; move(base, event.key === 'Home' ? -offset : 6 - offset); }
    });
    prev.addEventListener('click', () => { view = new Date(view.getFullYear(), view.getMonth() - 1, 1); focusKey = null; render(); });
    next.addEventListener('click', () => { view = new Date(view.getFullYear(), view.getMonth() + 1, 1); focusKey = null; render(); });
    today.addEventListener('click', () => { const now = new Date(); if (inRange(new Date(now.getFullYear(), now.getMonth(), now.getDate()))) set(iso(now)); });
    clear.addEventListener('click', () => set(''));
    shared.onOpen = () => { const selected = parseDate(input.value) || (inRange(new Date()) ? new Date() : parseDate(input.min) || parseDate(input.max) || new Date()); view = new Date(selected.getFullYear(), selected.getMonth(), 1); focusKey = null; render(); };
    return { wrapper, open: shared.show, close: () => shared.hide(true), refresh() { shared.sync(); if (shared.isOpen()) { render(); shared.position(); } }, destroy: shared.destroy };
  }

  /* Time: hour and minute columns. The minute step follows the input's step attribute (seconds), default 5 minutes. */
  function mountTime(input) {
    const wrapper = wrap(input, 'time'), shared = popupFor(input, wrapper, 'time', 'Tijd kiezen', 'schedule'), popup = shared.popup;
    const columns = el('div', 't-picker-columns'), hours = el('div', 't-picker-column'), minutes = el('div', 't-picker-column');
    hours.setAttribute('role', 'listbox'); hours.setAttribute('aria-label', 'Uur'); minutes.setAttribute('role', 'listbox'); minutes.setAttribute('aria-label', 'Minuten');
    columns.append(hours, minutes);
    const foot = el('div', 't-picker-foot'), now = textButton('Nu'), clear = textButton('Wissen', 't-action--neutral');
    foot.append(now, clear); popup.append(columns, foot);
    const stepMinutes = () => { const s = parseFloat(input.step); return Number.isFinite(s) && s > 0 ? Math.max(1, Math.round(s / 60)) : 5; };
    const value = (h, m) => `${pad(h)}:${pad(m)}`;
    const allowed = v => !(input.min && v < input.min.slice(0, 5)) && !(input.max && v > input.max.slice(0, 5));
    function cell(column, text, data, selected, disabled) {
      const button = el('button', 't-picker-cell', text); button.type = 'button'; button.setAttribute('role', 'option'); button.dataset.value = String(data);
      button.setAttribute('aria-selected', String(selected)); button.disabled = disabled; button.tabIndex = -1; column.append(button); return button;
    }
    function render() {
      const current = parseTime(input.value), step = stepMinutes();
      hours.replaceChildren(); minutes.replaceChildren();
      for (let h = 0; h < 24; h++) cell(hours, pad(h), h, current?.h === h, !allowed(value(h, 0)) && !allowed(value(h, 59)));
      const list = []; for (let m = 0; m < 60; m += step) list.push(m); if (current && !list.includes(current.m)) list.push(current.m); list.sort((a, b) => a - b);
      for (const m of list) cell(minutes, pad(m), m, current?.m === m, current ? !allowed(value(current.h, m)) : false);
      for (const column of [hours, minutes]) (column.querySelector('[aria-selected="true"]') || column.querySelector('.t-picker-cell:not(:disabled)'))?.setAttribute('tabindex', '0');
      clear.hidden = input.required; clear.disabled = !input.value;
    }
    function apply(next, close) { if (input.value !== next) { input.value = next; emit(input); } if (close) shared.hide(true); }
    hours.addEventListener('click', event => { const button = event.target.closest('.t-picker-cell'); if (!button || button.disabled) return; const current = parseTime(input.value); apply(value(+button.dataset.value, current?.m ?? 0), false); render(); hours.querySelector(`[data-value="${button.dataset.value}"]`)?.focus({ preventScroll: true }); minutes.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'center' }); });
    minutes.addEventListener('click', event => { const button = event.target.closest('.t-picker-cell'); if (!button || button.disabled) return; const current = parseTime(input.value); apply(value(current?.h ?? new Date().getHours(), +button.dataset.value), true); });
    columns.addEventListener('keydown', event => {
      const button = event.target.closest('.t-picker-cell'); if (!button) return; const column = button.parentElement;
      if (event.key === 'ArrowUp' || event.key === 'ArrowDown') { event.preventDefault(); event.stopPropagation(); const cells = [...column.querySelectorAll('.t-picker-cell:not(:disabled)')]; const next = cells[cells.indexOf(button) + (event.key === 'ArrowDown' ? 1 : -1)]; if (next) { column.querySelectorAll('[tabindex="0"]').forEach(node => node.tabIndex = -1); next.tabIndex = 0; next.focus({ preventScroll: true }); next.scrollIntoView({ block: 'nearest' }); } }
      else if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') { event.preventDefault(); event.stopPropagation(); (column === hours ? minutes : hours).querySelector('[tabindex="0"]')?.focus({ preventScroll: true }); }
    });
    now.addEventListener('click', () => { const d = new Date(), step = stepMinutes(); let m = Math.round(d.getMinutes() / step) * step, h = d.getHours(); if (m >= 60) { m = 0; h = (h + 1) % 24; } apply(value(h, m), true); });
    clear.addEventListener('click', () => apply('', true));
    shared.onOpen = render;
    return { wrapper, open: shared.show, close: () => shared.hide(true), refresh() { shared.sync(); if (shared.isOpen()) { render(); shared.position(); } }, destroy: shared.destroy };
  }

  /* Number: plus above minus in one joined column. stepUp/stepDown keep min, max and step exact; holding repeats. */
  function mountNumber(input) {
    const wrapper = wrap(input, 'number'), minus = iconButton('remove', 'Minder'), plus = iconButton('add', 'Meer'), stack = el('span', 't-picker-stack');
    minus.tabIndex = -1; plus.tabIndex = -1; stack.append(plus, minus); wrapper.append(stack);
    const hadInputMode = input.hasAttribute('inputmode');
    const decimalStep = () => input.step === 'any' || /\.\d*[1-9]/.test(input.step) || /\.\d*[1-9]/.test(input.value);
    function sync() {
      const locked = input.matches(':disabled') || input.readOnly, v = parseFloat(input.value);
      minus.disabled = locked || (!Number.isNaN(v) && input.min !== '' && v <= parseFloat(input.min));
      plus.disabled = locked || (!Number.isNaN(v) && input.max !== '' && v >= parseFloat(input.max));
      if (!hadInputMode) input.setAttribute('inputmode', decimalStep() ? 'decimal' : 'numeric');
    }
    function manual(direction) {
      const step = input.step === 'any' || !(parseFloat(input.step) > 0) ? 1 : parseFloat(input.step);
      const decimals = Math.max(String(step).split('.')[1]?.length || 0, String(input.value).split('.')[1]?.length || 0);
      let next = (parseFloat(input.value) || 0) + direction * step;
      if (input.min !== '' && next < parseFloat(input.min)) next = parseFloat(input.min);
      if (input.max !== '' && next > parseFloat(input.max)) next = parseFloat(input.max);
      input.value = next.toFixed(decimals);
    }
    function stepBy(direction) {
      if (input.matches(':disabled') || input.readOnly) return;
      const before = input.value;
      try { direction > 0 ? input.stepUp() : input.stepDown(); } catch { manual(direction); }
      if (input.value !== before) emit(input);
      sync();
    }
    function hold(button, direction) {
      let delay, repeat; const stop = () => { clearTimeout(delay); clearInterval(repeat); };
      button.addEventListener('pointerdown', event => {
        event.preventDefault(); if (event.button !== 0 || button.disabled) return;
        if (!input.disabled) input.focus({ preventScroll: true });
        stepBy(direction); button.setPointerCapture?.(event.pointerId);
        delay = setTimeout(() => { repeat = setInterval(() => button.disabled ? stop() : stepBy(direction), 70); }, 400);
      });
      for (const type of ['pointerup', 'pointercancel', 'lostpointercapture']) button.addEventListener(type, stop);
      button.addEventListener('keydown', event => { if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); stepBy(direction); } });
      return stop;
    }
    const stops = [hold(minus, -1), hold(plus, 1)];
    input.addEventListener('input', sync);
    const observer = new MutationObserver(sync); observer.observe(input, { attributes: true, attributeFilter: ['disabled', 'readonly', 'min', 'max', 'step', 'value'] }); sync();
    return { wrapper, open() {}, close() {}, refresh: sync, destroy() { stops.forEach(stop => stop()); observer.disconnect(); input.removeEventListener('input', sync); if (!hadInputMode) input.removeAttribute('inputmode'); } };
  }

  function mount(input) {
    if (instances.has(input) || !input.matches(SELECTOR) || input.parentElement?.classList.contains('t-input-action')) return;
    const kind = input.type; if (!['date', 'time', 'number'].includes(kind)) return;
    const part = kind === 'date' ? mountDate(input) : kind === 'time' ? mountTime(input) : mountNumber(input);
    instances.set(input, { input, kind, wrapper: part.wrapper, open: part.open, close: part.close, refresh: part.refresh, destroy() {
      part.destroy(); if (input.parentElement === part.wrapper && part.wrapper.parentNode) { part.wrapper.before(input); part.wrapper.remove(); } instances.delete(input);
    } });
  }
  function scan(root = document) {
    if (root.matches?.(SELECTOR)) mount(root);
    root.querySelectorAll?.(SELECTOR).forEach(mount);
  }
  function start() {
    scan(); new MutationObserver(records => {
      for (const record of records) for (const node of record.addedNodes) if (node.nodeType === 1) scan(node);
      for (const item of instances.values()) if (!item.input.isConnected) item.destroy();
    }).observe(document.documentElement, { childList: true, subtree: true });
  }
  globalThis.TactilePicker = Object.freeze({ mount: scan, get: input => instances.get(input) });
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start, { once: true }); else start();
})();
