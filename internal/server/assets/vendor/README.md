# Vendored browser dependencies

These files are pinned and served by Spin itself; the UI does not load a CDN.

| Package | Version | Browser file | License |
| --- | ---: | --- | --- |
| [Marked](https://marked.js.org/) | 18.0.11 | `marked-18.0.11.js` | MIT (`LICENSE.marked`) |
| [DOMPurify](https://github.com/cure53/DOMPurify) | 3.4.14 | `dompurify-3.4.14.min.js` | Apache-2.0 (`LICENSE.dompurify`) |
| [Mermaid](https://mermaid.js.org/) | 11.17.2 | `mermaid-11.17.2.min.js` | MIT (`LICENSE.mermaid`) |
| [Material Symbols](https://fonts.google.com/icons) | variable font (full) | `material-symbols-outlined.css` + `.woff2` | Apache-2.0 (`LICENSE.material-symbols`) |
| [markdown](https://github.com/xinix00/markdown) | 1.6.1 | `markdown-1.6.1.js` + `markdown-1.6.1.css` | MIT (`LICENSE.markdown`) |
| [xterm.js](https://xtermjs.org/) | 5.5.0 | `xterm-5.5.0.js` + `xterm-5.5.0.css` | MIT (`LICENSE.xterm`) |
| [@xterm/addon-fit](https://github.com/xtermjs/xterm.js) | 0.10.0 | `xterm-addon-fit-0.10.0.js` | MIT (`LICENSE.xterm`) |

Update deliberately: verify the upstream version and license, replace the
versioned file, then update the reference in `ui.html` or `assets/spin.js` and
the asset tests. Also increment `frontendAssetVersion` in `internal/server/ui.go`;
all browser assets are deliberately cached forever under that versioned path.

The full Material Symbols font is vendored from Google's
`material-design-icons/variablefont/MaterialSymbolsOutlined[FILL,GRAD,opsz,wght].woff2`.
SHA-256: `329f6eb34ac05b0c0b1bb172e36d004bbc57cb5112abeeccc70755afdc4f2d8d`.

## Shared Tactile components

`tactile-elements.css`, `tactile-select.js` and `tactile-markdown.js` are local copies from Haasstyle.
Keep their contents identical to that design library. The stylesheet supplies
generic supplementary materials, elevation, hover and editor theming. The
adapter preserves textarea form values, reset, validation and dynamic cleanup.
They are separate from the unchanged upstream Markdown 1.6.1 files.

The select adapter enhances single and multiple selects with a shared flat
listbox, keyboard controls and optional search. The original select owns form
values, validation, reset and events, including dynamically rendered fields.
Menu options use `t-choice`; gloss belongs to the trigger, not the menu rows.

Material Symbols Outlined is the shared Tactile icon family. The Markdown
adapter replaces toolbar SVGs without changing upstream editor code. Icon-only
actions share `--t-icon-button-size` (36 px, 44 px on touch/small screens) and
`--t-icon-size` (20 px), including file, label and notification close buttons.

Haasstyle application components are vendored from `/Users/derek/Git/haasstyle`:
`tactile-theme.css`, `tactile-elements.css`, `tactile-components.css`,
`tactile-select.js`, `tactile-markdown.js`, `tactile-dialog.js`, `tactile-combobox.js`.
The component source and its live examples live in Haasstyle. Make material
changes there first and sync the exact files here; spin.css only owns layout.
