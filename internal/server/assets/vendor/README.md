# Vendored browser dependencies

These files are pinned and served by Spin itself; the UI does not load a CDN.

| Package | Version | Browser file | License |
| --- | ---: | --- | --- |
| [Marked](https://marked.js.org/) | 18.0.11 | `marked-18.0.11.js` | MIT (`LICENSE.marked`) |
| [DOMPurify](https://github.com/cure53/DOMPurify) | 3.4.14 | `dompurify-3.4.14.min.js` | Apache-2.0 (`LICENSE.dompurify`) |
| [Mermaid](https://mermaid.js.org/) | 11.17.2 | `mermaid-11.17.2.min.js` | MIT (`LICENSE.mermaid`) |
| [Material Symbols](https://fonts.google.com/icons) | variable font (full) | `material-symbols-outlined.css` + `.woff2` | Apache-2.0 (`LICENSE.material-symbols`) |

Update deliberately: verify the upstream version and license, replace the
versioned file, then update the asset route in `ui.html` and the asset tests.

The full Material Symbols font is vendored from Google's
`material-design-icons/variablefont/MaterialSymbolsOutlined[FILL,GRAD,opsz,wght].woff2`.
SHA-256: `329f6eb34ac05b0c0b1bb172e36d004bbc57cb5112abeeccc70755afdc4f2d8d`.
