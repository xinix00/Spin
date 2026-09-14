# Server en interface

De server vertaalt HTTP- en WebSocket-verzoeken naar de bestaande store,
workflow en capsule-engine. Routes staan in `server.go`; domeinregels blijven
in `internal/store` en `internal/domain`.

| Onderdeel | Verantwoordelijkheid |
| --- | --- |
| `server.go` | Server opbouwen, routes, middleware en HTTP-handlers |
| `launch.go` | Achtergrondstarts, retries, annuleren, voortgang en opruimen |
| `state.go` | Gebruikerszicht op de state en updates via WebSocket |
| `connections.go` | HTTP-handlers voor Git-accounts, repositories en MCP |
| `workflow.go`, `workflow_actions.go` | Workflow-uitvoering en systeemacties |
| `ui.go`, `ui.html` | Ingebedde dashboardpagina en versiebeheer van assets |
| `assets/spin.js` | Kleine loader, ook voor eerder gecachte dashboardpagina's |
| `assets/ui/app.js` | Applicatiestate, interacties, formulieren en schermen |
| `assets/ui/navigation.js` | Hoofdnavigatie en subtabs met opgeslagen selectie |
| `assets/ui/render.js` | Gedeelde Markdown-, Mermaid- en codeweergave |

De browser gebruikt native JavaScript-modules zonder buildstap. Imports zijn
relatief, zodat alle modules onder dezelfde `/assets/v<versie>/` blijven.
De loader blijft een klassiek script voor bestaande dashboardpagina's.
Nieuwe modules staan onder `assets/ui/` en worden automatisch ingebed.
Verhoog bij iedere frontendwijziging `frontendAssetVersion` in `ui.go`.
HTML en API-antwoorden blijven niet-cachebaar; alleen actuele versie-assets
krijgen immutable caching.

Een gewone start en een retry gebruiken dezelfde opruimlogica. Die verwijdert
alleen de eigen registratie: een oude start die nog afrondt mag zijn opvolger
niet uit de voortgang verwijderen of annuleren.

`app.js` bevat nog gedeelde schermstate. Verdere opsplitsing kan per functiegebied
(chat, reviews, recorder, backup) zodra de afhankelijkheden expliciet gemaakt
worden. Houd pure weergavefuncties in `render.js` vrij van applicatiestate en
voorkom dat schermmodules elkaars globale variabelen gaan aanpassen.

Controleer frontendwijzigingen met `node --check` op de loader en gewijzigde
modules en met `go test ./internal/server`. Na wijzigingen aan gedeelde
serverlogica draait ook `go test ./...`. De assettests controleren zowel de
actuele versie als de fallback voor eerder geopende dashboards.
