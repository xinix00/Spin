/*! @xinix00/markdown v1.7.0 | MIT | https://github.com/xinix00/markdown */
(function (global) {
    'use strict';

    // -----------------------------------------------------------------------
    // Block parsing — every line is a block, the prefix decides the style
    // -----------------------------------------------------------------------

    const RE = /^(#{1,3} |[-*] |\d+\. |> ?|```)/;
    const TYPES = {
        '# ': ['h1', '#'], '## ': ['h2', '##'], '### ': ['h3', '###'],
        '- ': ['bullet', '•'], '* ': ['bullet', '•'], '> ': ['quote', null], '>': ['quote', null], '```': ['code', '</>'],
    };

    function parse(text) {
        const m = text.match(RE);
        const prefix = m ? m[1] : '';
        const content = prefix ? text.slice(prefix.length) : text;
        const t = TYPES[prefix];
        if (t) return { prefix, content, type: t[0], dec: t[1] };
        if (prefix) return { prefix, content, type: 'numbered', dec: prefix.trim() };
        if (text[0] === '|') return { prefix: '', content: text, type: 'table', dec: null };
        if (IMAGE_RE.test(text.trim())) return { prefix: '', content: text, type: 'image', dec: null };
        return { prefix: '', content: text, type: 'p', dec: null };
    }

    const isListPrefix = (prefix) => prefix === '- ' || prefix === '* ' || /^\d+\. $/.test(prefix);
    const isQuotePrefix = (prefix) => prefix === '> ' || prefix === '>';

    // Raw markdown ↔ blocks. A list item may span several lines: in markdown the
    // extra lines are indented under the marker ("- a\n  b"); in the editor they are
    // one block with a line break. Everything else is one line = one block.
    function textToBlocks(text) {
        const blocks = [];
        let indent = 0; // indent that continues the previous list block, 0 = none
        for (const line of text.split('\n')) {
            if (indent && line.length > indent && line.slice(0, indent).trim() === '' && line.trim() !== '') {
                blocks[blocks.length - 1] += '\n' + line.slice(indent);
                continue;
            }
            blocks.push(line);
            const { prefix } = parse(line);
            indent = isListPrefix(prefix) ? prefix.length : 0;
        }
        return blocks.length ? blocks : [''];
    }

    function blockToText(block) {
        const { prefix } = parse(block);
        return isListPrefix(prefix) ? block.replace(/\n/g, '\n' + ' '.repeat(prefix.length)) : block;
    }
    const blocksToText = (blocks) => blocks.map(blockToText).join('\n');

    // A block that is exactly one image: ![alt](src "optional title")
    const IMAGE_RE = /^!\[([^\]]*)\]\(\s*(\S+?)(?:\s+"[^"]*")?\s*\)$/;
    function parseImage(text) {
        const m = text.trim().match(IMAGE_RE);
        return m ? { alt: m[1], src: m[2] } : null;
    }

    // -----------------------------------------------------------------------
    // Tables — a block starting with "|" is a table row. Contiguous rows are
    // re-aligned (padded) when you leave the table so the pipes line up.
    // -----------------------------------------------------------------------

    const isSepCell = (c) => /^:?-+:?$/.test(c);

    // Format a group of table rows so all pipes line up. `active` = { row, caret }
    // for the row being typed in: everything left of the caret is kept verbatim,
    // spaces right of the caret count as padding. Returns the new rows and caret.
    // With `fill`, short rows get empty cells so the table becomes rectangular.
    function formatTable(rows, active, fill) {
        const parsed = rows.map((row, ri) => {
            const segs = row.slice(1).replace(/\\\|/g, '\u0000').split('|'); // \| = escaped pipe inside a cell
            if (segs.length > 1 && segs[segs.length - 1].trim() === '') segs.pop();
            let pos = 1, activeCell = -1, off = 0;
            const contents = segs.map((seg, c) => {
                seg = seg.replace(/\u0000/g, '\\|');
                const start = pos;
                pos += seg.length + 1;
                if (active && ri === active.row && activeCell < 0 && active.caret >= start && active.caret <= start + seg.length) {
                    const left = seg.slice(0, active.caret - start), right = seg.slice(active.caret - start).trimEnd();
                    let content = left + right;
                    off = left.length;
                    if (content[0] === ' ') { content = content.slice(1); off = Math.max(0, off - 1); }
                    activeCell = c;
                    return content;
                }
                return seg.trim();
            });
            const sep = contents.length > 0 && contents.every(isSepCell);
            return { contents, sep, activeCell, off };
        });
        const cols = Math.max(1, ...parsed.map((r) => r.contents.length));
        const width = Array.from({ length: cols }, (_, c) =>
            Math.max(3, ...parsed.map((r) => (r.sep ? 0 : (r.contents[c] || '').length))));
        let caret = null;
        const out = parsed.map((r) => {
            const n = fill ? cols : r.contents.length;
            const cellsOut = [];
            for (let c = 0; c < n; c++) {
                const content = r.contents[c] || '';
                if (r.sep) {
                    const left = content.startsWith(':'), right = content.endsWith(':');
                    cellsOut.push((left ? ':' : '-') + '-'.repeat(Math.max(1, width[c] - 2)) + (right ? ':' : '-'));
                } else cellsOut.push(content + ' '.repeat(Math.max(0, width[c] - content.length)));
                if (c === r.activeCell) {
                    let at = 2;
                    for (let k = 0; k < c; k++) at += width[k] + 3;
                    caret = at + Math.min(r.off, width[c]);
                }
            }
            return '| ' + cellsOut.join(' | ') + ' |';
        });
        return { rows: out, caret };
    }

    // Cells of a raw row (\| inside a cell is an escaped pipe)
    function parseCells(row) {
        const segs = row.slice(1).replace(/\\\|/g, '\u0000').split('|');
        if (segs.length > 1 && segs[segs.length - 1].trim() === '') segs.pop();
        return segs.map((c) => c.trim().replace(/\u0000/g, '|').replace(/<br\s*\/?>/gi, '\n'));
    }
    const isSepRow = (row) => { const c = parseCells(row); return c.length > 0 && c.every(isSepCell); };
    // Cells → raw row: pipes escaped, line breaks inside a cell become <br> (GFM convention)
    const rowFrom = (values) => '| ' + values.map((v) => v.trim().replace(/\|/g, '\\|').replace(/\n/g, '<br>')).join(' | ') + ' |';

    // -----------------------------------------------------------------------
    // Toolbar — inline SVG icons (Lucide, ISC license)
    // -----------------------------------------------------------------------

    const ICONS = {
        bold: '<path d="M6 12h9a4 4 0 0 1 0 8H7a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1h7a4 4 0 0 1 0 8"/>',
        italic: '<line x1="19" x2="10" y1="4" y2="4"/><line x1="14" x2="5" y1="20" y2="20"/><line x1="15" x2="9" y1="4" y2="20"/>',
        strike: '<path d="M16 4H9a3 3 0 0 0-2.83 4"/><path d="M14 12a4 4 0 0 1 0 8H6"/><line x1="4" x2="20" y1="12" y2="12"/>',
        h1: '<path d="M4 12h8"/><path d="M4 18V6"/><path d="M12 18V6"/><path d="m17 12 3-2v8"/>',
        h2: '<path d="M4 12h8"/><path d="M4 18V6"/><path d="M12 18V6"/><path d="M21 18h-4c0-4 4-3 4-6 0-1.5-2-2.5-4-1"/>',
        h3: '<path d="M4 12h8"/><path d="M4 18V6"/><path d="M12 18V6"/><path d="M17.5 10.5c1.7-1 3.5 0 3.5 1.5a2 2 0 0 1-2 2"/><path d="M17 17.5c2 1.5 4 .3 4-1.5a2 2 0 0 0-2-2"/>',
        bullet: '<path d="M3 5h.01"/><path d="M3 12h.01"/><path d="M3 19h.01"/><path d="M8 5h13"/><path d="M8 12h13"/><path d="M8 19h13"/>',
        numbered: '<path d="M11 5h10"/><path d="M11 12h10"/><path d="M11 19h10"/><path d="M4 4h1v5"/><path d="M4 9h2"/><path d="M6.5 20H3.4c0-1 2.6-1.925 2.6-3.5a1.5 1.5 0 0 0-2.6-1.02"/>',
        quote: '<path d="M16 3a2 2 0 0 0-2 2v6a2 2 0 0 0 2 2 1 1 0 0 1 1 1v1a2 2 0 0 1-2 2 1 1 0 0 0-1 1v2a1 1 0 0 0 1 1 6 6 0 0 0 6-6V5a2 2 0 0 0-2-2z"/><path d="M5 3a2 2 0 0 0-2 2v6a2 2 0 0 0 2 2 1 1 0 0 1 1 1v1a2 2 0 0 1-2 2 1 1 0 0 0-1 1v2a1 1 0 0 0 1 1 6 6 0 0 0 6-6V5a2 2 0 0 0-2-2z"/>',
        code: '<path d="m16 18 6-6-6-6"/><path d="m8 6-6 6 6 6"/>',
        table: '<path d="M12 3v18"/><rect width="18" height="18" x="3" y="3" rx="2"/><path d="M3 9h18"/><path d="M3 15h18"/>',
        paragraph: '<path d="M13 4v16"/><path d="M17 4v16"/><path d="M19 4H9.5a4.5 4.5 0 0 0 0 9H13"/>',
    };

    // [id, action, argument] — null = divider
    const TOOLBAR = [
        ['bold', 'wrap', '**'], ['italic', 'wrap', '*'], ['strike', 'wrap', '~~'], null,
        ['h1', 'setPrefix', '# '], ['h2', 'setPrefix', '## '], ['h3', 'setPrefix', '### '], null,
        ['bullet', 'setPrefix', '- '], ['numbered', 'setPrefix', '1. '], ['quote', 'setPrefix', '> '], null,
        ['code', 'setPrefix', '```'], ['table', 'insertTable', null], ['paragraph', 'setPrefix', ''],
    ];

    const LABELS = {
        bold: 'Bold', italic: 'Italic', strike: 'Strikethrough',
        h1: 'Heading 1', h2: 'Heading 2', h3: 'Heading 3',
        bullet: 'Bullet list', numbered: 'Numbered list', quote: 'Quote',
        code: 'Code', table: 'Table', paragraph: 'Paragraph',
        placeholder: 'Type here…',
    };

    function icon(name) {
        const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
        svg.setAttribute('viewBox', '0 0 24 24');
        svg.setAttribute('fill', 'none');
        svg.setAttribute('stroke', 'currentColor');
        svg.setAttribute('stroke-width', '2');
        svg.setAttribute('stroke-linecap', 'round');
        svg.setAttribute('stroke-linejoin', 'round');
        svg.setAttribute('aria-hidden', 'true');
        svg.innerHTML = ICONS[name];
        return svg;
    }

    // Browsers without `field-sizing: content` need a JS auto-grow fallback
    const AUTO_GROW = !(global.CSS && CSS.supports && CSS.supports('field-sizing', 'content'));
    function fit(ta) {
        if (!AUTO_GROW || ta.tagName !== 'TEXTAREA') return;
        ta.style.height = 'auto';
        ta.style.height = ta.scrollHeight + 'px';
    }

    // -----------------------------------------------------------------------
    // Component — Alpine.js data object (also usable via mount())
    // -----------------------------------------------------------------------

    function component(options) {
        const opts = Object.assign({ toolbar: true }, options || {});
        const labels = Object.assign({}, LABELS, opts.labels || {});
        if (opts.placeholder !== undefined) labels.placeholder = opts.placeholder;
        // Image sources can be translated (e.g. an id → a real URL) via a map or a function
        const imageUrl = (src) => (typeof opts.imageUrl === 'function' ? opts.imageUrl(src) : (opts.images && opts.images[src]) || src);

        return {
            blocks: [], active: 0,
            selFrom: null, selTo: null, mouseDown: false, dragFrom: null,

            init() {
                const root = this.$el;
                root.classList.add('md-editor');
                root.setAttribute('tabindex', '-1'); // focusable for block-selection keys

                this.source = root.querySelector('textarea');
                if (!this.source) throw new Error('md-editor: no <textarea> inside root element');
                this.source.style.display = 'none';

                if (opts.toolbar) root.insertBefore(this.buildToolbar(), this.source);
                this.container = document.createElement('div');
                this.container.className = 'md-blocks';
                root.insertBefore(this.container, this.source);

                this.blocks = textToBlocks(this.source.value || '');
                this.$nextTick(() => this.render(false));

                // Mouse drag across blocks selects a range. Browsers capture the mouse for the
                // textarea during a text-selection drag, so we track the pointer on the document.
                this._onMouseMove = (e) => {
                    if (!this.mouseDown || this.dragFrom === null) return;
                    const to = this.rowAt(e.clientY);
                    if (to !== this.dragFrom || this.selRange()) this.select(this.dragFrom, to);
                };
                this._onMouseUp = () => { this.mouseDown = false; this.dragFrom = null; };
                document.addEventListener('mousemove', this._onMouseMove);
                document.addEventListener('mouseup', this._onMouseUp);

                this._onFocusOut = (e) => {
                    if (root.contains(e.relatedTarget)) return;
                    this.clearSelection();
                };
                root.addEventListener('focusout', this._onFocusOut);

                // Keyboard while a block range is selected (focus is on the root element)
                this._onKeyDown = (e) => {
                    const range = this.selRange();
                    if (!range || e.target !== root) return; // textarea keys are handled in buildBlock()
                    const mod = e.metaKey || e.ctrlKey;
                    const key = e.key.length === 1 ? e.key.toLowerCase() : e.key;
                    if (key === 'Escape') { const to = this.selTo; this.clearSelection(); this.focus(to, 'end'); }
                    else if (key === 'Backspace' || key === 'Delete') { this.deleteSelection(); }
                    else if (key === 'ArrowUp' || key === 'ArrowDown') {
                        const dir = key === 'ArrowUp' ? -1 : 1;
                        if (e.shiftKey) this.select(this.selFrom, this.selTo + dir);
                        else { const to = this.selTo; this.clearSelection(); this.focus(to, dir < 0 ? 'start' : 'end'); }
                    }
                    else if (mod && key === 'c') { this.copySelection(); }
                    else if (mod && key === 'x') { this.copySelection(); this.deleteSelection(); }
                    else if (mod && key === 'a') { this.selectAll(); }
                    else if (mod) { return; } // let other shortcuts (paste, undo…) through
                    else if (key === 'Enter') { this.deleteSelection(); }
                    else if (e.key.length === 1 && !e.altKey) { this.replaceSelection(e.key); }
                    else return;
                    e.preventDefault();
                };
                root.addEventListener('keydown', this._onKeyDown);

                // Clipboard events (Edit menu / context menu) while a block range is selected
                this._onCopy = (e) => {
                    const range = this.selRange();
                    if (!range) return;
                    e.preventDefault();
                    e.clipboardData.setData('text/plain', this.selectedText());
                    if (e.type === 'cut') this.deleteSelection();
                };
                root.addEventListener('copy', this._onCopy);
                root.addEventListener('cut', this._onCopy);
                this._onPaste = (e) => {
                    const range = this.selRange();
                    if (!range) return;
                    e.preventDefault();
                    this.replaceSelection(e.clipboardData.getData('text'));
                };
                root.addEventListener('paste', this._onPaste);
            },

            destroy() {
                document.removeEventListener('mousemove', this._onMouseMove);
                document.removeEventListener('mouseup', this._onMouseUp);
                const root = this.$el;
                if (!root) return;
                root.removeEventListener('focusout', this._onFocusOut);
                root.removeEventListener('keydown', this._onKeyDown);
                root.removeEventListener('copy', this._onCopy);
                root.removeEventListener('cut', this._onCopy);
                root.removeEventListener('paste', this._onPaste);
            },

            buildToolbar() {
                const bar = document.createElement('div');
                bar.className = 'md-toolbar';
                TOOLBAR.forEach((item) => {
                    if (!item) {
                        const div = document.createElement('span');
                        div.className = 'md-divider';
                        bar.appendChild(div);
                        return;
                    }
                    const [id, action, arg] = item;
                    const btn = document.createElement('button');
                    btn.type = 'button';
                    btn.title = labels[id];
                    btn.setAttribute('aria-label', labels[id]);
                    btn.dataset.md = id;
                    btn.appendChild(icon(id));
                    btn.addEventListener('mousedown', (e) => e.preventDefault()); // keep textarea focus
                    btn.addEventListener('click', () => this[action](arg));
                    bar.appendChild(btn);
                });
                return bar;
            },

            render(autoFocus = true) {
                const c = this.container;
                c.innerHTML = '';
                let group = null; // consecutive table rows share one horizontally scrolling wrapper
                this.blocks.forEach((text, i) => {
                    const row = this.buildBlock(text, i);
                    if (parse(text).type === 'table') {
                        if (!group) {
                            group = document.createElement('div');
                            group.className = 'md-table-group';
                            group.style.setProperty('--md-cols', this.tableCols(i)); // read by grid-template-columns on the group
                            c.appendChild(group);
                        }
                        group.appendChild(row);
                    } else { group = null; c.appendChild(row); }
                });
                if (AUTO_GROW) c.querySelectorAll('textarea').forEach(fit);
                if (autoFocus) this.focus(this.active);
            },

            buildBlock(text, i) {
                const { prefix, content, type, dec } = parse(text);
                const isHeading = type[0] === 'h';

                const row = document.createElement('div');
                row.className = 'md-block md-' + type;

                if (dec !== null) {
                    const d = document.createElement('span');
                    d.className = 'md-decorator';
                    d.textContent = dec;
                    row.appendChild(d);
                }

                // Start of a possible drag-selection (see _onMouseMove)
                row.addEventListener('mousedown', (e) => {
                    if (e.button !== 0) return;
                    this.clearSelection();
                    this.mouseDown = true;
                    this.dragFrom = i;
                });

                if (type === 'table') { this.buildTableRow(row, i, content); return row; }

                const ta = document.createElement('textarea');
                ta.className = 'md-input';
                ta.value = content;
                ta.rows = 1;
                if (i === 0 && !content) ta.placeholder = labels.placeholder;

                ta.addEventListener('focus', () => {
                    this.active = i; this.cell = null;
                    if (!this.mouseDown) this.clearSelection();
                });
                ta.addEventListener('input', () => {
                    fit(ta);
                    this.blocks[i] = prefix + ta.value;
                    this.sync();
                    // Auto-detect a freshly typed prefix ("- ", ">") or a type change ("|" → table row, "![..](..)" → image)
                    const parsed = parse(prefix + ta.value);
                    if (parsed.type === 'image' && type === 'image') { this.updateImage(ta.parentNode.querySelector('.md-img'), ta.value); return; }
                    if ((parsed.prefix !== prefix && parsed.prefix) || parsed.type !== type) {
                        if (parsed.type === 'table') this.prettyTable(i);
                        this.render(false);
                        this.focus(i, 'end');
                    }
                });

                ta.addEventListener('keydown', (e) => {
                    const mod = e.metaKey || e.ctrlKey;
                    const last = this.blocks.length - 1;
                    const atStart = !ta.selectionStart && !ta.selectionEnd;
                    const firstNl = ta.value.indexOf('\n'), lastNl = ta.value.lastIndexOf('\n');
                    const onFirstLine = firstNl === -1 || ta.selectionStart <= firstNl;
                    const onLastLine = lastNl === -1 || ta.selectionEnd > lastNl;

                    const k = e.key.length === 1 ? e.key.toLowerCase() : e.key;

                    if (mod && !e.altKey && k === 'b') { e.preventDefault(); this.wrap('**'); }
                    else if (mod && !e.altKey && k === 'i') { e.preventDefault(); this.wrap('*'); }
                    else if (mod && e.shiftKey && (k === 's' || k === 'x')) { e.preventDefault(); this.wrap('~~'); }
                    else if (e.key === 'Enter') {
                        // Shift+Enter = line break inside a list item (indented continuation line in markdown).
                        // Everywhere else it would produce the same markdown as Enter, so it just is Enter.
                        if (e.shiftKey && isListPrefix(prefix)) return;
                        e.preventDefault(); this.split(i, ta);
                    }
                    else if (e.shiftKey && e.key === 'ArrowUp' && onFirstLine && i > 0) { e.preventDefault(); this.select(i, i - 1); }
                    else if (e.shiftKey && e.key === 'ArrowDown' && onLastLine && i < last) { e.preventDefault(); this.select(i, i + 1); }
                    else if (mod && e.key.toLowerCase() === 'a' && last > 0 && ta.selectionStart === 0 && ta.selectionEnd === ta.value.length) {
                        e.preventDefault(); this.selectAll(); // second Cmd/Ctrl+A (or on an empty block) selects all blocks
                    }
                    else if (e.key === 'Backspace' && atStart && i > 0) { e.preventDefault(); this.merge(i); }
                    else if (e.key === 'ArrowUp' && !ta.selectionStart && i > 0) { e.preventDefault(); this.focus(i - 1, 'end'); }
                    else if (e.key === 'ArrowDown' && ta.selectionStart === ta.value.length && i < last) { e.preventDefault(); this.focus(i + 1, 'start'); }
                });

                ta.addEventListener('paste', (e) => {
                    const paste = e.clipboardData.getData('text');
                    if (!paste.includes('\n')) return;
                    e.preventDefault();
                    const before = ta.value.slice(0, ta.selectionStart), after = ta.value.slice(ta.selectionEnd);
                    const nl = paste.indexOf('\n');
                    this.blocks[i] = prefix + before + paste.slice(0, nl);
                    const rest = textToBlocks(paste.slice(nl + 1));
                    if (after) rest[rest.length - 1] += after;
                    this.blocks.splice(i + 1, 0, ...rest);
                    this.active = i + rest.length;
                    this.render(); this.sync();
                });

                row.appendChild(ta);
                if (type === 'image') row.appendChild(this.buildImage(content));
                return row;
            },

            // The real image, shown under the raw ![alt](src) line; hidden while it does not load
            buildImage(text) {
                const img = document.createElement('img');
                img.className = 'md-img';
                img.addEventListener('error', () => { img.hidden = true; });
                img.addEventListener('load', () => { img.hidden = false; });
                this.updateImage(img, text);
                return img;
            },

            updateImage(img, text) {
                const im = parseImage(text);
                if (!im) { img.hidden = true; return; }
                const url = imageUrl(im.src);
                if (img.getAttribute('src') !== url) { img.hidden = true; img.src = url; }
                img.alt = im.alt;
            },

            // Re-resolve every image (call after changing the images map)
            refreshImages() {
                this.rows().forEach((row, i) => { const img = row.querySelector('.md-img'); if (img) this.updateImage(img, this.blocks[i]); });
            },

            split(i, ta) {
                const before = ta.value.slice(0, ta.selectionStart), after = ta.value.slice(ta.selectionStart);
                const { prefix } = parse(this.blocks[i]);
                const continues = isListPrefix(prefix) || isQuotePrefix(prefix); // lists and quotes carry their marker to the next line

                this.blocks[i] = prefix + before;

                if (continues && !before.trim()) { this.blocks[i] = ''; this.blocks.splice(i + 1, 0, after); } // Enter on an empty item ends it
                else if (/^\d+\. /.test(prefix)) this.blocks.splice(i + 1, 0, (parseInt(prefix, 10) + 1) + '. ' + after);
                else if (continues) this.blocks.splice(i + 1, 0, prefix + after);
                else this.blocks.splice(i + 1, 0, after);

                this.active = i + 1; this.render(); this.sync();
            },

            merge(i) {
                const prev = parse(this.blocks[i - 1]), curr = parse(this.blocks[i]);
                const cursor = prev.content.length;
                this.blocks[i - 1] = prev.prefix + prev.content + curr.content;
                this.blocks.splice(i, 1);
                this.active = i - 1; this.render();
                this.$nextTick(() => {
                    const ta = this.ta(i - 1);
                    if (ta) { ta.focus(); ta.selectionStart = ta.selectionEnd = cursor; }
                });
                this.sync();
            },

            // Wrap the selection in `syntax`, or remove the markers if it is already wrapped.
            // Without a selection: insert the markers with the caret in between.
            wrap(syntax) {
                const ta = this.ta(this.active);
                if (!ta) return;
                const v = ta.value, n = syntax.length;
                let s = ta.selectionStart, e = ta.selectionEnd, sel = v.slice(s, e);
                if (sel.length >= 2 * n && sel.startsWith(syntax) && sel.endsWith(syntax)) {
                    sel = sel.slice(n, -n); ta.value = v.slice(0, s) + sel + v.slice(e); e = s + sel.length;
                } else if (s >= n && v.slice(s - n, s) === syntax && v.slice(e, e + n) === syntax) {
                    ta.value = v.slice(0, s - n) + sel + v.slice(e + n); s -= n; e = s + sel.length;
                } else {
                    ta.value = v.slice(0, s) + syntax + sel + syntax + v.slice(e);
                    s += n; e = s + sel.length;
                }
                ta.setSelectionRange(s, e);
                if (ta.classList.contains('md-cell')) {
                    this.blocks[this.active] = rowFrom([...ta.closest('.md-block').querySelectorAll('.md-cell')].map((el) => el.value));
                    this.prettyTable(this.active);
                } else this.blocks[this.active] = parse(this.blocks[this.active]).prefix + ta.value;
                ta.focus(); fit(ta); this.sync();
            },

            setPrefix(p) {
                this.blocks[this.active] = p + parse(this.blocks[this.active]).content;
                this.render(false); this.sync();
                this.focus(this.active, 'end');
            },

            // --- Tables ------------------------------------------------------

            insertTable() {
                const i = this.active;
                const rows = ['| Column 1 | Column 2 |', '| -------- | -------- |', '|          |          |'];
                const replace = this.blocks[i].trim() === '';
                this.blocks.splice(replace ? i : i + 1, replace ? 1 : 0, ...rows);
                this.render(false); this.sync();
                this.focus(replace ? i : i + 1, { cell: 0 });
            },

            tableRange(i) {
                let a = i, b = i;
                while (a > 0 && parse(this.blocks[a - 1]).type === 'table') a--;
                while (b < this.blocks.length - 1 && parse(this.blocks[b + 1]).type === 'table') b++;
                return [a, b];
            },

            // Keep the raw markdown of the table around block i aligned and rectangular
            prettyTable(i) {
                const [a, b] = this.tableRange(i);
                formatTable(this.blocks.slice(a, b + 1), null, true).rows.forEach((row, k) => { this.blocks[a + k] = row; });
            },

            // Number of columns of the table around block i
            tableCols(i) {
                const [a, b] = this.tableRange(i);
                return Math.max(1, ...this.blocks.slice(a, b + 1).map((r) => parseCells(r).length));
            },

            // A table row is a grid of <input>s, one per cell. Tab moves between them natively.
            buildTableRow(row, i, text) {
                const [a] = this.tableRange(i);
                const cols = this.tableCols(i);
                if (isSepRow(text)) { row.classList.add('md-table-sep'); return; }
                if (i === a) row.classList.add('md-table-head');
                const values = parseCells(text);
                for (let c = 0; c < cols; c++) {
                    const input = document.createElement('textarea'); // same element as every other block: multi-line, auto-growing
                    input.className = 'md-input md-cell';
                    input.rows = 1;
                    input.value = values[c] || '';
                    const sizeCols = () => { input.cols = Math.max(3, ...input.value.split('\n').map((l) => l.length + 1)); };
                    sizeCols();
                    input.addEventListener('focus', () => { this.active = i; this.cell = c; if (!this.mouseDown) this.clearSelection(); });
                    input.addEventListener('input', () => {
                        sizeCols(); fit(input);
                        this.blocks[i] = rowFrom([...row.querySelectorAll('.md-cell')].map((el) => el.value));
                        this.prettyTable(i);
                        this.sync();
                    });
                    input.addEventListener('keydown', (e) => {
                        const mod = e.metaKey || e.ctrlKey, k = e.key.length === 1 ? e.key.toLowerCase() : e.key;
                        const atStart = input.selectionStart === 0 && input.selectionEnd === 0;
                        const atEnd = input.selectionStart === input.value.length && input.selectionEnd === input.value.length;
                        const rowEmpty = [...row.querySelectorAll('.md-cell')].every((el) => el.value === '');
                        const firstNl = input.value.indexOf('\n'), lastNl = input.value.lastIndexOf('\n');
                        const onFirstLine = firstNl === -1 || input.selectionStart <= firstNl;
                        const onLastLine = lastNl === -1 || input.selectionEnd > lastNl;
                        if (mod && !e.altKey && k === 'b') { e.preventDefault(); this.wrap('**'); }
                        else if (mod && !e.altKey && k === 'i') { e.preventDefault(); this.wrap('*'); }
                        else if (mod && e.shiftKey && (k === 's' || k === 'x')) { e.preventDefault(); this.wrap('~~'); }
                        else if (e.key === '|' && !mod) { e.preventDefault(); this.insertColumn(i, c, input); }
                        else if (e.key === 'Tab' && !e.shiftKey && c === cols - 1 && i === this.blocks.length - 1) {
                            // Tab out of the very last cell with nothing below: create a line under the table
                            e.preventDefault();
                            this.blocks.push('');
                            this.render(false); this.sync(); this.focus(i + 1, 'start');
                        }
                        else if (e.key === 'Enter') { if (e.shiftKey) return; e.preventDefault(); this.tableEnter(i, rowEmpty); } // Shift+Enter = newline in the cell
                        else if (e.key === 'Backspace' && atStart && c === 0 && rowEmpty) { e.preventDefault(); this.removeTableRow(i); }
                        else if (e.key === 'Backspace' && atStart && c > 0 && input.value === '') {
                            e.preventDefault();
                            if (this.columnEmpty(i, c)) this.removeColumn(i, c); else this.focus(i, { cell: c - 1, at: 'end' });
                        }
                        else if (e.key === 'ArrowLeft' && atStart && c > 0) { e.preventDefault(); this.focus(i, { cell: c - 1, at: 'end' }); }
                        else if (e.key === 'ArrowRight' && atEnd && c < cols - 1) { e.preventDefault(); this.focus(i, { cell: c + 1, at: 'start' }); }
                        else if (e.shiftKey && e.key === 'ArrowUp' && onFirstLine && i > 0) { e.preventDefault(); this.select(i, i - 1); }
                        else if (e.shiftKey && e.key === 'ArrowDown' && onLastLine && i < this.blocks.length - 1) { e.preventDefault(); this.select(i, i + 1); }
                        else if (e.key === 'ArrowUp' && onFirstLine && i > 0) { e.preventDefault(); this.focus(i - 1, { cell: c, at: 'end' }); }
                        else if (e.key === 'ArrowDown' && onLastLine && i < this.blocks.length - 1) { e.preventDefault(); this.focus(i + 1, { cell: c, at: 'start' }); }
                    });
                    row.appendChild(input);
                    fit(input);
                }
            },

            // Index of the first data row (after header + separator). A table always keeps
            // its header and one data row; to remove a table, select its rows and press Backspace.
            firstDataRow(i) {
                const [first] = this.tableRange(i);
                return isSepRow(this.blocks[first + 1] || '') ? first + 2 : first + 1;
            },

            // Enter in a cell: new row below (after the header the "---" separator comes first);
            // Enter on an empty last row ends the table, unless it is the only data row.
            tableEnter(i, rowEmpty) {
                const [first, last] = this.tableRange(i);
                if (rowEmpty && i === last && i > this.firstDataRow(i)) {
                    this.blocks[i] = '';
                    this.render(false); this.sync(); this.focus(i, 'end');
                    return;
                }
                const cols = Math.max(1, parseCells(this.blocks[i]).length);
                const add = [];
                let at = i + 1;
                if (i === first) {
                    if (isSepRow(this.blocks[i + 1] || '')) at = i + 2; // never insert between header and separator
                    else add.push('| ' + Array(cols).fill('---').join(' | ') + ' |');
                }
                add.push('| ' + Array(cols).fill('   ').join(' | ') + ' |');
                this.blocks.splice(at, 0, ...add);
                this.prettyTable(i);
                this.render(false); this.sync();
                this.focus(at + add.length - 1, { cell: 0 });
            },

            // "|" typed in a cell: split the cell there and give every row a new column after it
            insertColumn(i, c, input) {
                const head = input.value.slice(0, input.selectionStart), tail = input.value.slice(input.selectionEnd);
                const [a, b] = this.tableRange(i);
                for (let k = a; k <= b; k++) {
                    const values = parseCells(this.blocks[k]);
                    while (values.length <= c) values.push('');
                    if (k === i) values[c] = head;
                    values.splice(c + 1, 0, isSepRow(this.blocks[k]) ? '---' : (k === i ? tail : ''));
                    this.blocks[k] = rowFrom(values);
                }
                this.prettyTable(i);
                this.render(false); this.sync();
                this.focus(i, { cell: c + 1, at: 'start' });
            },

            columnEmpty(i, c) {
                const [a, b] = this.tableRange(i);
                for (let k = a; k <= b; k++) {
                    if (isSepRow(this.blocks[k])) continue;
                    if ((parseCells(this.blocks[k])[c] || '') !== '') return false;
                }
                return true;
            },

            // Backspace in an empty cell whose whole column is empty removes that column
            removeColumn(i, c) {
                const [a, b] = this.tableRange(i);
                for (let k = a; k <= b; k++) {
                    const values = parseCells(this.blocks[k]);
                    if (values.length > 1 && c < values.length) values.splice(c, 1);
                    this.blocks[k] = rowFrom(values);
                }
                this.prettyTable(i);
                this.render(false); this.sync();
                this.focus(i, { cell: c - 1, at: 'end' });
            },

            // After a block edit (selection deleted / replaced) every table must still be
            // header + separator + at least one data row. A leading separator without a
            // header is dropped, a missing separator is inserted, a missing data row is added.
            repairTables() {
                for (let i = 0; i < this.blocks.length; i++) {
                    if (parse(this.blocks[i]).type !== 'table') continue;
                    while (i < this.blocks.length && parse(this.blocks[i]).type === 'table' && isSepRow(this.blocks[i])) this.blocks.splice(i, 1);
                    if (i >= this.blocks.length || parse(this.blocks[i]).type !== 'table') continue;
                    const cols = Math.max(1, parseCells(this.blocks[i]).length);
                    if (!isSepRow(this.blocks[i + 1] || '')) this.blocks.splice(i + 1, 0, '| ' + Array(cols).fill('---').join(' | ') + ' |');
                    if (parse(this.blocks[i + 2] || '').type !== 'table') this.blocks.splice(i + 2, 0, '| ' + Array(cols).fill('   ').join(' | ') + ' |');
                    this.prettyTable(i);
                    i = this.tableRange(i)[1];
                }
            },

            // Backspace in an empty row removes it — never the header or the first data row
            removeTableRow(i) {
                if (i <= this.firstDataRow(i)) return;
                this.blocks.splice(i, 1);
                if (parse(this.blocks[i - 1] || '').type === 'table') this.prettyTable(i - 1);
                this.render(false); this.sync();
                this.focus(i - 1, { cell: 0, at: 'end' });
            },

            // pos = 'start' | 'end' | caret offset | { cell, at } for table rows
            focus(i, pos) {
                i = Math.max(0, Math.min(i, this.blocks.length - 1));
                const row = this.rows()[i];
                if (!row) return;
                if (row.classList.contains('md-table-sep')) { // separator rows are not editable: step over them
                    const down = pos === 'start' || (pos && pos.at === 'start');
                    const next = down ? i + 1 : i - 1;
                    if (next >= 0 && next < this.blocks.length) this.focus(next, pos);
                    return;
                }
                this.active = i;
                const cellsEls = row.querySelectorAll('.md-cell');
                let el;
                if (cellsEls.length) {
                    const want = pos && typeof pos === 'object' ? pos.cell : (pos === 'end' ? cellsEls.length - 1 : 0);
                    el = cellsEls[Math.max(0, Math.min(want ?? 0, cellsEls.length - 1))];
                    pos = pos && typeof pos === 'object' ? (pos.at || 'end') : pos;
                } else {
                    el = row.querySelector('textarea');
                    if (pos && typeof pos === 'object') pos = pos.at || 'end';
                }
                if (!el) return;
                el.focus();
                const at = pos === 'end' ? el.value.length : pos === 'start' ? 0 : pos;
                if (typeof at === 'number') el.setSelectionRange(at, at);
                this.$nextTick(() => el.scrollIntoView({ behavior: 'smooth', block: 'nearest' }));
            },

            // --- Block selection -------------------------------------------
            // A range of whole blocks. While active, focus sits on the root element and
            // the keyboard/clipboard handlers in init() operate on the raw markdown lines.

            select(from, to) {
                const last = this.blocks.length - 1;
                from = Math.max(0, Math.min(from, last));
                to = Math.max(0, Math.min(to, last));
                if (from === to) { this.clearSelection(); this.focus(from, 'end'); return; }
                this.selFrom = from; this.selTo = to;
                this.$el.classList.add('md-selecting');
                this.$el.focus(); // moves focus off the textarea; relatedTarget is inside root so focusout keeps the selection
                window.getSelection?.().removeAllRanges();
                const [a, b] = this.selRange();
                this.rows().forEach((row, i) => row.classList.toggle('md-selected', i >= a && i <= b));
                this.rows()[to]?.scrollIntoView({ block: 'nearest' });
            },

            selectAll() { this.select(0, this.blocks.length - 1); },

            selRange() {
                if (this.selFrom === null || this.selTo === null || this.selFrom === this.selTo) return null;
                return [Math.min(this.selFrom, this.selTo), Math.max(this.selFrom, this.selTo)];
            },

            selectedText() {
                const range = this.selRange();
                return range ? blocksToText(this.blocks.slice(range[0], range[1] + 1)) : '';
            },

            clearSelection() {
                this.selFrom = this.selTo = null;
                this.$el?.classList.remove('md-selecting');
                if (this.container) this.rows().forEach((r) => r.classList.remove('md-selected'));
            },

            rowAt(clientY) {
                const rows = this.rows();
                for (let i = 0; i < rows.length; i++) {
                    if (clientY <= rows[i].getBoundingClientRect().bottom) return i;
                }
                return rows.length - 1;
            },

            copySelection() {
                const text = this.selectedText();
                const fallback = () => {
                    const ta = document.createElement('textarea');
                    ta.value = text; ta.style.position = 'fixed'; ta.style.opacity = '0';
                    document.body.appendChild(ta); ta.select();
                    try { document.execCommand('copy'); } catch (_) { /* ignore */ }
                    ta.remove(); this.$el.focus();
                };
                if (navigator.clipboard?.writeText) navigator.clipboard.writeText(text).catch(fallback);
                else fallback();
            },

            // Replace the selected blocks with the given text (one block per line)
            replaceSelection(text) {
                const range = this.selRange();
                if (!range) return;
                const lines = textToBlocks(text);
                this.blocks.splice(range[0], range[1] - range[0] + 1, ...lines);
                this.active = range[0] + lines.length - 1;
                this.repairTables();
                this.clearSelection(); this.render(false); this.sync();
                this.focus(this.active, 'end');
            },

            deleteSelection() { this.replaceSelection(''); },

            // --- Helpers ---------------------------------------------------

            rows() { return [...this.container.querySelectorAll('.md-block')]; },
            ta(i) { const r = this.rows()[i]; return r ? (r.querySelector('.md-input:focus') || r.querySelector('.md-input')) : undefined; },
            sync() {
                this.source.value = blocksToText(this.blocks);
                this.source.dispatchEvent(new Event('input', { bubbles: true }));
            },
            value() { return blocksToText(this.blocks); },
        };
    }

    // Vanilla usage without Alpine: MarkdownEditor.mount(el, options)
    function mount(el, options) {
        const c = component(options);
        c.$el = el;
        c.$nextTick = (fn) => Promise.resolve().then(fn);
        c.init();
        return c;
    }

    // Register with Alpine if it shows up (works with the CSP build too)
    document.addEventListener('alpine:init', () => {
        if (global.Alpine && global.Alpine.data) global.Alpine.data('markdownEditor', component);
    });

    global.markdownEditor = component;
    global.MarkdownEditor = { component, mount, parse, parseImage, formatTable, textToBlocks, blocksToText, labels: LABELS, version: '1.7.0' };
})(window);
