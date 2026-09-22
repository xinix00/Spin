# SPIN-interface · Haasstyle / Tactile

De interface gebruikt Haasstyle met drie materialen: Classic 95, Glossy 08
en Tactile Matte. Glossy 08 in donkere modus is de standaard. De bron van de
gedeelde componenten is `../haasstyle`; kopieën staan in
`internal/server/assets/vendor/`. Er is geen buildstap of CDN nodig.

## Opbouw

`spin.css` importeert vendorfonts, de lokale Markdown 1.6.1 CSS in de vendorlaag,
het thema, materialen en componenten. De laatste laag `appearance` bevat de
gedeelde, op `data-style` begrensde Classic- en Matte-materialen.
De applicatielaag bevat de shell, pagina-indeling, workflowstatussen en de
geometrie van documenten, code review en terminal. Knopglans, invoervelden,
selectiestates, dialogen en iconen komen uit de gedeelde bestanden.

- `t-action`: glossy actie; `t-icon-button`: vaste vierkante icoonknop.
- `t-choice`: vlakke navigatie of menuoptie; selectie met accentlijn.
  `t-choice--marked` gebruikt de aanwezige vink/radiomarkering zonder extra lijn.
- `t-segmented`: donkere accentkleur, lichtere actieve keuze, geen hoverreactie.
- `t-field`, `t-fieldset`, `t-check-row`: invoer en formuliergroepen.
- `t-panel`, `t-card`, `t-well`: rustige buitenpanelen, records en verzonken inhoud.
- `t-dialog` en `t-dialog--fullscreen`: formulieren en grote werkvensters.
- `t-disclosure`, `t-prose`, `t-table`, `t-code`: activiteit en rijke inhoud.
- `t-label`/`t-tag`: informatieve labels, verloop zonder knop-schaduw.

`ui.html` en templates in `ui/app.js` benoemen hun componenten expliciet.
Er is geen algemene DOM-observer die aan de hand van appklassen knoppen restylet.
De adapters voor selects, suggesties en Markdown beheren wel hun eigen lifecycle.

## Bediening

`TactileSelect` vervangt de zichtbare native single/multiselect, maar behoudt het
originele veld voor FormData, validatie en reset. `TactileCombobox` doet hetzelfde
voor vrije invoer met `input[list]`. Escape sluit eerst de keuzelijst.

`TactileDialog.confirm()` retourneert een Promise<boolean>. Wacht altijd op het
resultaat voordat een actie wordt uitgevoerd. Sluiten en Escape annuleren;
meerdere vragen worden na elkaar getoond. Gebruik een concrete `confirmLabel`
bij nieuwe acties. `TactileDialog.notice()` vervangt blokkerende browsermeldingen.

`TactileMarkdown` verbindt xinix00/markdown met de bestaande textarea en
formulierafhandeling. Material Symbols Outlined wordt lokaal geladen.

## Dekking

De shell en het linkermenu, Jobs en Templates, Explore, Environments,
Git/MCP/Runners, Access, alle formulieren, logins, bestanden en bijlagen,
chat/toolberichten/toestemmingen, documenten/comments, code review, terminal,
herstelvensters en meldingen gebruiken deze basis. Brede werkvensters hebben
hun eigen scrollgebieden. Op mobiel komen kolommen onder elkaar; de navigatie
blijft beschikbaar als horizontale rij.

## Wijzigen en controleren

Ontbreekt een generiek element, voeg het eerst toe aan de Haasstyle-galerij en
guidelines. Kopieer vervolgens de gedeelde bestanden ongewijzigd naar vendor.
Verhoog `frontendAssetVersion` in `internal/server/ui.go` bij frontendwijzigingen.
Behoud `preventCaching` voor HTML/API-antwoorden.

Verplicht: `node --check internal/server/assets/spin.js` en
`go test ./internal/server`. Controleer daarnaast de gewijzigde modules met
`node --check`, en de betrokken schermen op desktop en mobiel. Neem dynamische
inhoud, toetsenbordbediening, annuleren, formulierwaarden/reset en lange teksten
mee. Live runners en herstel van echte databases vragen hun eigen functionele
testomgeving; visuele fixtures starten zulke acties niet.

## Weergavevoorkeuren

Via **Weergave** (ook vóór inloggen) kies je onafhankelijk het thema, één van
vijf themakleuren en de lichte of donkere modus. `ui/appearance.js` valideert en
bewaart die voorkeur onder `spin-appearance` en past hem vóór de eerste paint toe.
Classic houdt grijze knoppen, met het accent op selectie. Matte gebruikt heldere
matte vullingen. Licht Glossy bouwt op van `#FAFAFA` naar `#F2F2F2` en `#E9E9E9`.
Terminal, syntaxkleuren, diffs en bestaande Mermaid-diagrammen volgen de modus.

Markdown gebruikt exact dezelfde lokale versie, adapter, materialen, iconen en
cascade als de Haasstyle-galerij; er is geen `@latest`-stylesheet die deze kan
overrulen. `tactile-picker.js` verzorgt ook dynamisch toegevoegde getal-, datum-
en tijdvelden. Alle laadindicaties gebruiken `.t-spinner`: dezelfde ring van
16 px, 2 px rand en 900 ms als de galerij, met respect voor reduced motion.
Schakelaars gebruiken een thumb van 16 px inclusief rand en 2 px vrije ruimte.
