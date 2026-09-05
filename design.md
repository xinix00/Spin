# EasyACP / Spin — ontwerp

## 1. De kern

Spin neemt een werkende omgeving op als een immutable snapshot. De inhoud is opaque: Spin hoeft Codex, Claude, DeepSeek of een toekomstige wrapper niet na te bouwen en hoeft hun loginformaat niet te kennen.

Een Artifact heeft drie onafhankelijke eigenschappen:

- een logische identiteit, bijvoorbeeld `tool:codex` of `credential:codex`;
- een scope, bijvoorbeeld `global` of `user`;
- optioneel een expliciete parent, die vertelt op welke eerdere snapshot deze laag voortbouwt.

Er is geen zichtbaar `base`-type meer. `alpine:3.24` is het schone, verwisselbare substraat van de Docker-engine. Node is gewoon een herbruikbare toollaag:

```text
alpine:3.24                  engine-substraat, geen Artifact
└── tool:git                verplichte bronlaag voor iedere Session
    └── tool:node           apk add nodejs npm
        └── tool:codex      install Codex CLI + codex-acp; ENABLES acp
            ├── credential:codex @ user:derek
            └── credential:codex @ user:john
```

De graph bepaalt de lagen. Iedere kind is mechanisch gelijk: `tool`, `credential` en toekomstige namen gebruiken dezelfde opname-, parent- en resolverlogica. `credential` krijgt alleen conservatieve metadata-defaults zoals `scope=user` en `sensitivity=secret`.

## 2. RECORD

De canonieke syntax gebruikt overal `kind:name`:

```text
RECORD tool:git --scope=global
apk add --no-cache git openssh-client ca-certificates
END RECORD

RECORD tool:node --scope=global --from=tool:git
apk add --no-cache nodejs npm
END RECORD

RECORD tool:codex --scope=global --from=tool:node --enable=acp --command=codex-acp
npm install -g @openai/codex @agentclientprotocol/codex-acp
codex --version
END RECORD

RECORD credential:codex --scope=user --from=tool:codex
codex login --device-auth
END RECORD
```

`--from` is voor geen enkel kind verplicht en er bestaat geen impliciete parentrelatie. In de praktijk neem je een vervolgsnapshot met `--from` op wanneer de nieuwe laag de bestaande filesystemstate nodig heeft.

Tijdens `RECORD` draait een echte container. Iedere niet-Spin-regel start met `docker exec -it` en `sh -lc` in een echte PTY. De browser en server houden daarvoor een duplex WebSocket open: uitvoer stroomt onmiddellijk naar de terminal, nieuwe invoer gaat naar stdin en Ctrl-C stuurt het terminalsignaal. Een Capsule mag maximaal acht van deze kanalen tegelijk hebben; daardoor kan een tweede proces een watcher, server of localhost-callback bedienen zonder het eerste proces te onderbreken. Na exit bewaart Spin alleen volgnummer, exitcode en tijdstip. Commandtekst en transcript zijn voor alle Recordingtypes vluchtig; de immutable snapshot is de waarheid. `END RECORD` maakt een Docker image commit en verwijdert de opnamecontainer. Het Artifact verwijst naar de immutable image.

## 3. Scope en identiteit

Spin-identiteit komt niet uit een vrij invulbaar requestveld. Bij first-run maakt de beheerder één lokale owner; daarna geeft een HttpOnly sessioncookie de server de actuele user. De server overschrijft ieder ontvangen `operator`/`actor`-veld met die username. Een admin kan in Access extra members of admins maken. Users worden nooit verwijderd: archiveren trekt atomair al hun login-sessions in en blokkeert nieuwe logins, maar behoudt hun identity en alle verwijzende audit- en workflowrecords. Herstellen activeert dezelfde identity opnieuw. Een admin kan zichzelf en de laatste actieve admin niet archiveren. Browsermutaties vereisen bovendien een per-session CSRF-token en een geldige same-origin request.

De logische naam blijft voor iedereen gelijk. Scope maakt de fysieke instantie verschillend:

```text
credential:codex / default / user:derek
credential:codex / default / user:john
tool:git / default / global
tool:git / default / user:john
```

Een kale selector wordt in deze volgorde opgelost:

1. `user:<huidige operator>`;
2. project;
3. team;
4. global.

Een user-scoped Artifact van iemand anders is niet zichtbaar voor de resolver en wordt nooit als fallback gebruikt. Een ander profiel kan met `--profile=work` worden geselecteerd.

## 4. USE is generiek

`USE` kiest precies één entry-Artifact en herstelt de volledige parentclosure:

```text
USE tool:codex
```

Dit start de toolketen zonder credentiallaag.

```text
USE credential:codex
```

Dit kiest automatisch de `credential:codex` van de huidige operator en start de snapshot waarin ook `tool:codex`, `tool:node` en het Alpine-substraat zitten.

Er is geen speciale auth-overlayresolver, geen `--auth=self` en geen hardcoded Codexgedrag. Elk nieuw Artifacttype kan op dezelfde manier een entrypoint zijn:

```text
USE config:corp-network
USE workspace:easyacp
USE result:candidate-a
```

De Docker-engine start een bestaande descendant-image direct wanneer die alle bindings bevat. Zijn er onafhankelijke branches, dan maakt hij vluchtig een composition-image door de minimaal benodigde snapshots in selectievolgorde als filesystem-union te streamen. Hierdoor blijven `USE` en `WITH` werkelijk generiek zonder snapshotinhoud te interpreteren.

Een Composition kan daarnaast expliciete extra bindings vragen:

```text
USE tool:codex WITH tool:dotnet WITH config:company
```

De eerste selector blijft de entry en bepaalt de agent/worker-routing. Iedere `WITH`-selector wordt onafhankelijk volgens operator, scope en profiel opgelost en daarna aan dezelfde slotmap toegevoegd. Daardoor verandert een buildtool niet per ongeluk de primaire agent. `requested_artifact_ids` bewaart de expliciete keuzes; `resolved_artifacts` bevat hun gezamenlijke parentclosure.

Docker kan meerdere gekozen snapshots alleen starten wanneer één snapshot alle andere als ancestors bevat. Een bruikbare opnamevolgorde is bijvoorbeeld:

```text
tool:node → tool:codex → tool:dotnet → credential:codex
```

`USE tool:codex WITH tool:dotnet` kiest dan de Dotnet-image als fysieke materialisatieroot, terwijl Codex logisch de entry blijft. Voor onafhankelijke images maakt Spin een tijdelijke union-image. Bestandsinhoud uit een latere selector wint bij conflicten; afwezige bestanden werken tussen onafhankelijke branches niet als tombstone. Gebruik daarom `--from=<earlier-layer>` voor exacte delete-semantiek. De union wordt rechtstreeks tussen Docker-processen gestreamd, als managed composition-image gelabeld en bij `STOP` verwijderd. Een mislukte materialisatie wordt direct weer uit de Composition-state verwijderd.

## 5. ENABLED

Een laag kan declareren wat hij mogelijk maakt. Dat staat los van zijn type:

```json
{
  "name": "acp",
  "command": "codex-acp",
  "transport": "stdio",
  "protocol_version": 1
}
```

De Composition publiceert `enabled`: de geërfde unie van alle lagen. Als een child dezelfde capability opnieuw publiceert, overschrijft diens launch-descriptor de parent. Spin bewaart onbekende capabilities opaque. Daardoor kan later een hook voor bijvoorbeeld `mcp`, `ssh`, `browser`, `vscode-extension` of een intern protocol worden toegevoegd zonder snapshotmigratie.

CLI:

```text
RECORD tool:codex --from=tool:node \
  --enable=acp --command=codex-acp --transport=stdio --protocol-version=1
```

Voor ACP zijn `stdio` en protocolversie `1` de defaults. Een command blijft verplicht om de hook daadwerkelijk te starten.

## 6. ACP-integratie

ACP v1 is de huidige stabiele protocolversie. Een client start een agent als subprocess; JSON-RPC 2.0-berichten zijn UTF-8, newline-delimited en lopen over stdin/stdout. Iedere verbinding begint met `initialize`, waarin beide kanten protocolversie, capabilities en auth-methodes uitwisselen.

De eerste ingebouwde hook werkt daarom als volgt:

```text
Composition ENABLED acp
        │
        ▼
Docker exec -i <capsule> sh -lc <acp command>
        │
        ▼
initialize(protocolVersion=1, clientInfo=EasyACP)
        │
        ▼
session/new(cwd=/workspace,
            additionalDirectories=[/root],
            mcpServers=user bindings)
        │
        ▼
session/prompt ⇄ session/update / request_permission
```

Gebruik:

```text
USE credential:codex
ACP PROBE
```

Of via HTTP:

```http
POST /api/compositions/{compositionID}/acp/probe
{"operator":"derek"}
```

De probe start de entrypoint in de gematerialiseerde snapshot, onderhandelt ACP v1 en sluit daarna het subprocess. Dit verifieert dus de echte laag, niet alleen de metadata. De normale GUI-flow gebruikt `GET /api/sessions/{id}/acp`: een WebSocket abonneert zich op een server-side supervisor die één ACP-subprocess per Spin Session in leven houdt. Het sluiten van het scherm verbreekt alleen de browserverbinding; de agent en een lopende beurt blijven actief. Heropenen speelt de vluchtige events van die serverruntime opnieuw af.

De browser verstuurt tekst als ACP-contentblocks, ontvangt Markdownchunks, plannen en tool-callupdates, en kan ACP-permissionopties of `session/cancel` terugsturen. Spin adverteert nog geen client-filesystem of client-terminalcapability: de agent werkt zelf binnen de Capsule. `/workspace` is de repository en `/root` is de HOME van de containeruser. Spin geeft die HOME als ACP `additionalDirectories` mee, zodat een workspace-write sandbox ook normale user-state en caches kan aanmaken zonder per-tool paden te kennen. Deze uitbreiding maakt alleen de materialiseerde Session schrijfbaar; de host en de snapshot waaruit zij ontstond blijven buiten bereik. Een aparte read-only workspace-inspector gebruikt Git voor status en `numstat`, zodat change counts niet uit modeltekst hoeven te worden afgeleid. De bestaande Job/Session/Result-objecten blijven de orchestratorlaag; ACP is de gestandaardiseerde agentverbinding daarbinnen.

## 7. Job, Session, Git en fork

Een Job beschrijft de gewenste uitkomst en is verplicht aan precies één `GitRepository` gekoppeld. Een Session is één uitvoeringslijn van een agent en bewaart dezelfde repository-ID, een primaire `environment_selector`, bijvoorbeeld `credential:codex`, plus optionele `with_selectors` voor buildtools en configuratielagen. Iedere nieuwe Job en Session wordt geweigerd wanneer die environmentclosure geen Artifact bevat dat `tool:git` aanbiedt. Pas bij `USE session:<id>` wordt de volledige selectie voor de actuele operator opgelost.

De Git-topologie is een vast onderdeel van het model:

```text
jobs/login-herstellen-a1b2c3/main                    Job.branch + reviewdoel
jobs/login-herstellen-a1b2c3/sessions/ses_primary    primary Session.git_ref
jobs/login-herstellen-a1b2c3/sessions/ses_worker     worker Session.git_ref
jobs/login-herstellen-a1b2c3/sessions/ses_reviewer   reviewer Session.git_ref
```

De Job- en Sessionrefs zijn siblings onder dezelfde namespace; `job/<naam>` plus `job/<naam>/<session>` zou ongeldig zijn omdat Git geen ref toestaat die prefix van een andere ref is. Elke Session heeft zowel `git_ref` als `target_branch`. Een nieuwe Job maakt automatisch de primary Session en kopieert de Job-`with_selectors`. Daarna maakt `POST /api/jobs/{jobID}/sessions` een willekeurige extra rol aan. Een child erft repository en Joblagen wanneer het veld is weggelaten, maar mag expliciet een andere environmentset kiezen. `spawned_by_session_id` is optioneel: bij een mens/orchestrator blijft hij leeg, bij agentdelegatie verwijst hij naar een bestaande Session van dezelfde Job. Dit is de generieke multi-agentprimitief; Spin hoeft niet te weten of de uitvoerder Codex, Claude of iets nieuws is.

Connections → Git bewaart remotes zonder ingebedde geheimen. Een repository bevat alleen providermetadata en `credential_scope=user|global`, nooit een account-ID. De control plane resolveert iedere checkout-, push- en PR-actie via de remote-host en scope: `user` kiest de provideridentity van de uitvoerende gebruiker en `global` het gedeelde service-account. `public` bestaat uitsluitend als migratiestatus voor oude ongekoppelde repositories. Een `GitAccount` is app-state en nadrukkelijk geen Artifact: alleen een Git-capabele snapshotlaag is voor de runtime vereist. GitHub/GitLab OAuth authorization-code + PKCE maakt of vernieuwt de identity; een tokeninvoer is de generieke fallback. Een admin kan een global identity en de provider-application configureren. Environmentconfiguratie blijft mogelijk en heeft voorrang.

Repositorynaam, remote-URL, default branch, credentialscope en projectlagen zijn samen bewerkbare repositoryconfiguratie. Bij Job-creatie bevriest Spin naam, remote, provider, scope en gekozen base branch in de Job. Een latere repository-edit beïnvloedt uitsluitend nieuwe Jobs; alle volgende Sessions en de PR-finalizer van een bestaande Job blijven de bevroren Git-configuratie gebruiken.

Bij materialisatie bevat de Composition uitsluitend repository-ID, credentialscope en geredigeerde provideridentiteit voor audit, geen accountbinding. De control plane resolveert het account opnieuw, haalt het geheim pas vlak voor checkout op en start een kortlevende, read-only helper vanaf de gekozen environment. Username, token en commitidentiteit gaan via stdin naar shellvariabelen; Git's vluchtige `credential.helper` leest ze zonder een executable credentialbestand te maken. Ze verschijnen niet in Docker-CLI-argumenten of containerconfig. De helper initialiseert of hervat een named volume, fetcht de vastgepinde Job-base, checkt de Sessionref uit en stelt lokale `user.name`/`user.email` in. Daarna start de agent-image met alleen dat volume op `/workspace`.

De GUI maakt dit één handeling: kies bij de Job een Template, repository en één opgenomen Git-capabele entry-Artifact en druk `Job inschieten`. De server bewaart eerst atomair de Job en eerste fase-Session en antwoordt direct met `202 Accepted`. Git-checkout, Docker-materialisatie en ACP-start draaien daarna buiten de HTTP-request op de achtergrond. De browser vergrendelt de submit tijdens de korte opslagactie en stuurt een stabiele idempotency-key; dezelfde request levert steeds dezelfde Job op en kan nooit een tweede achtergrondstart naast de eerste plannen. Als starten mislukt blijft de duurzame queued Job/Session bestaan en kan dezelfde start veilig opnieuw worden aangeboden.

Jobverwijdering is lokale lifecycle-cleanup: Spin annuleert eerst een nog lopende achtergrondstart, sluit ACP, stopt iedere runtime en verwijdert daarna atomair Job, Sessions, compositions, activations, turns, checkpoints, results en workflowbijlagen. Repositoryconfiguratie en remote Git-refs vallen buiten deze destructieve grens en blijven herstelbaar aanwezig. Een Job die context levert aan een vervolg-Job kan niet worden verwijderd zolang die verwijzing bestaat.

Een gesloten Job kan door iedere ingelogde user worden geforkt als nieuwe vervolg-Job. Dit is geen processcheckpoint: de nieuwe Job wordt eigendom van de forkende user, krijgt een eigen branch, eigen Templateflow en diens laat gebonden Git-identity, maar zijn `base_ref` en repository worden server-side vastgezet op de remote resultaatbranch van de bron. De oorspronkelijke goal, alle bronbijlagen en de laatste revisie van ieder bron-deliverable worden als read-only ACP-context meegegeven. Daarmee blijven feedback en nagekomen bugs onderdeel van dezelfde inhoudelijke lijn zonder een afgesloten workflow weer mutabel te maken.

Hierdoor kan John een Job starten en Derek hem overnemen zonder Johns Git-account te lenen: iedere actie resolveert opnieuw op repositoryscope en uitvoerende gebruiker. Een checkpoint-fork en een vers aangemaakte Job-Session komen allebei als afzonderlijke Session bij dezelfde Job terecht. De eerste bewaart continuïteit met een checkpoint; de tweede begint doelbewust vanuit de gekozen environment. De orchestrator kan dus worker-, criticus-, reviewer- en synthesevarianten plannen met dezelfde primitief.

Een Session moet eindigen in een Result met samenvatting, testbewijs, acceptatiebewijs, open punten en usage. Daarmee kan de orchestrator branches vergelijken zonder alle verborgen agentcontext te begrijpen.

Een `WorkflowTemplate` is uitsluitend data: een geordende set fasen met instructies, deliverabledefinities, een expliciete lijst te injecteren eerdere deliverable-namen, een `allow_changes`-vlag, een ACCEPT-route, een REJECT-route en rejectlimiet. Namen als Ontwikkeling en Bugfix hebben geen serverbetekenis. De Job Goal gaat naar iedere fase; geselecteerde deliverables gaan als hun laatste revisie en als verplichte context mee. Iedere `PhaseRun` krijgt een eigen Session en start vanaf de actuele Job-branch. De interne workflow-MCP-server biedt uitsluitend `ask`, optioneel `add_deliverable`, `accept` en `reject`. Commit is geen agentactie.

Agentfasen mogen elk een eigen `USE`-environment en reeks `WITH`-lagen kiezen; zonder override gebruiken ze het Job-recept. Bij het opslaan voegt Spin altijd een niet-bewerkbare `Pull request`-finalizer toe en routeert ieder `DONE` daarheen. Deze control-plane-actie resolveert de provideridentity uit repositoryscope en Job-user, nooit uit een credentiallaag in de agent-Capsule. Voor GitHub wordt `jobs/<naam-id>/main` de head, de oorspronkelijke basisbranch de base, de Job-naam de titel en de volledige Goal de body. Een API-fout wordt beperkt opnieuw geprobeerd en eindigt daarna als pending `RETRY PR`; de Job krijgt pas status `KLAAR` nadat de provider een PR heeft teruggegeven.

`ASK USER` is afzonderlijk overgangsbeleid op ACCEPT en REJECT en geen derde uitkomst. De agent eindigt altijd met ACCEPT of REJECT, waarbij REJECT een reden vereist. Met de gate op de gekozen route aan, of zodra de rejectlimiet is bereikt, bewaart Spin eerst `PendingOutcome` en toont de UI zowel de AI-uitkomst als de concrete ACCEPT- en REJECT-doelstaat. Daardoor kan ACCEPT menselijke goedkeuring vereisen terwijl REJECT automatisch terugloopt. De mens gebruikt exact hetzelfde beslismodel: ACCEPT volgt de voorwaartse route, REJECT met een eigen reden de terug-route. CHAT muteert bij openen niets; het eerste nieuwe bericht hervat dezelfde ACP-Session terwijl de wachtende beslissing open blijft. Eindigt de beurt van de agent zonder nieuw besluit, dan valt de fase terug op diezelfde beslissing en zijn ACCEPT en REJECT weer klikbaar; een nieuw accept of reject van de agent vervangt de openstaande beslissing. Er is dus geen verborgen JA/NEE-vertaling en een gesprek is nooit zelf een uitkomst.

De remote Job-branch is de gedeelde workflowstack. De checkout-helper maakt `jobs/<slug-id>/main` vóór de eerste Session atomair vanaf de gekozen basisref en materialiseert iedere Session vanaf de actuele remote Job-head. Alleen de control plane publiceert bij definitieve ACCEPT de Session-HEAD naar die Job-branch, met het laat gebonden Git-account van de operator. In dezelfde draaiende Capsule vergelijkt `WorkspaceAcceptor` HEAD en de volledige worktree met de bij checkout vastgelegde `spin.baseCommit`. Bij `allow_changes=false` blokkeert ieder verschil ACCEPT. Bij `allow_changes=true` worden dirty files en eventueel door de agent gemaakte commits teruggevouwen tot één resultaatcommit met Job-, Session-, fase- en acceptortrailers. Een bestaande Job-ref mag uitsluitend fast-forwarden; een ondertussen gewijzigde Job-branch blokkeert de integratie in plaats van geschiedenis te overschrijven. Na push leest Spin de remote ref terug en vereist exact dezelfde commit voordat workflowstate naar accepted gaat of een volgende Session start. Het token wordt alleen vluchtig aan Git gegeven en bereikt de agent niet. Documentfasen kunnen benoemde, gereviseerde Markdown-deliverables produceren; verplichte deliverables blokkeren ACCEPT totdat ze bestaan.

Een `DeliverableComment` is immutable en wijst naar precies één eveneens immutable Deliverable-ID/revisie. De tekstselector bewaart render-offsets, de exacte quote en prefix/suffix voor robuuste weergave; auteur en tijd komen uitsluitend van de serveridentiteit. Alleen de actuele laatste revisie van dezelfde Job en deliverablename accepteert nieuwe comments. Een volgende revisie krijgt een nieuw ID en dus vanzelf geen comments, terwijl de oudere revisie haar werkelijke commentgeschiedenis behoudt. Er bestaat geen resolve-status en ACCEPT/REJECT verwijst niet naar comments. De promptbuilder voegt alleen comments van actuele laatste revisies als onafhankelijke context toe.

## 8. Persoonlijke MCP-handoff

MCP-configuratie is geen snapshotlaag: het is user-scoped verbindingsmateriaal dat de ACP-client bij `session/new` aan de gekozen agent geeft. Een Job verwijst naar MCP-IDs; de primary Session erft ze. Nieuwe Sessions erven standaard de Jobset, maar mogen een eigen subset kiezen. Bij `USE session:<id>` kopieert de Composition die IDs, zodat de langdurige ACP-verbinding precies weet wat hij moet overdragen.

Het opgeslagen model volgt rechtstreeks de twee ACP-vormen:

```json
{"name":"github","command":"/usr/local/bin/github-mcp","args":["--stdio"],"env":[{"name":"GITHUB_TOKEN","value":"…"}]}
{"type":"http","name":"internal","url":"https://mcp.example.test","headers":[{"name":"Authorization","value":"…"}]}
```

Stdio-commands moeten absoluut zijn. HTTP wordt alleen doorgegeven wanneer de agent die MCP-transportcapability tijdens `initialize` aanbiedt. De server bewaart secretwaarden als AES-256-GCM-enveloppen, maar `GET /api/state` en create/delete-responses redigeren alle waarden. Bij het openen van de ACP Session haalt de supervisor de private definities voor de ingelogde operator op en geeft ze aan `session/new`; de browser ziet ze nooit. De API filtert user-scoped MCP-, Git- en Recordingmetadata op de ingelogde operator; de GUI hoeft die securitygrens dus niet zelf af te dwingen.

Het live transcript is expres geen nieuw opslagobject: chunks en tooloutput blijven alleen in het geheugen van de huidige serverruntime. De immutable Capsule en Git-workspace blijven de waarheid. Het ACP-session-ID duurzaam koppelen aan een checkpoint en na een serverrestart via `session/load`/`session/resume` herstellen is de volgende continuity-stap.

## 9. Wat een Docker-snapshot wel en niet bewaart

Een image commit bewaart:

- filesystemstate;
- geïnstalleerde binaries en wrappers;
- config- en credentialbestanden in de container;
- lokale agenthistorie voor zover die op disk staat;
- de exacte parentketen.

Een image commit bewaart niet:

- RAM of een draaiend proces;
- open sockets en file descriptors;
- provider-side sessiestate;
- provider-side KV/prompt cache.

Freezen vergroot de kans dat dezelfde lokale prefix en bestanden opnieuw aangeboden kunnen worden, maar garandeert geen cache-hit bij de provider. Een echte processcheckpointengine (bijvoorbeeld CRIU of een micro-VM snapshot) kan later naast Docker worden toegevoegd; daarom maakt `CapsuleSnapshot.includes_process_state` deze grens expliciet.

## 10. Server en runnerfleet

De deploymentgrens volgt de werkelijke verantwoordelijkheden:

```text
Browser ── HTTPS/WS ──> spin-server (GUI, state, orchestration, OAuth)
                            │
                            │ authenticated WSS control + multiplexed streams
                  ┌─────────┼─────────┐
                  ▼         ▼         ▼
              runner A  runner B  runner C
              Docker A  Docker B  Docker C
```

De servercontainer mount geen Docker-socket. `spin-client` bezit de concrete `capsule.Engine` en draait op een laptop, buildserver of goedkope computenode. Hij maakt uitsluitend een uitgaande WebSocket naar `GET /api/runner/ws`; er hoeft dus geen inkomende runnerpoort door NAT of een laptopfirewall. De browser blijft alleen met de server praten. Hetzelfde gemultiplexte protocol draagt korte capsule-RPC's, binaire PTY/ACP-stdioframes en opaque snapshotstreams.

Iedere client heeft een persistent random `instance_id`. De server vertaalt die één keer naar een duurzame `Client.id`; een nieuwe fysieke socket vervangt alleen de verbinding van dat object. Zowel server als client sturen iedere tien seconden een WebSocket Ping, antwoorden via Pong en verbreken een kanaal dat 35 seconden niets meer bevestigt. De client reconnect met begrensde exponential backoff en jitter. Request-ID's zijn idempotent: de server mag een onbeantwoorde opdracht na reconnect opnieuw sturen; de client koppelt die aan de lopende uitvoering of herhaalt zijn gecachete response.

Round-robin is uitsluitend plaatsingsbeleid voor een nieuwe workload. `CapsuleRuntime.client_id`, `CapsuleSnapshot.client_id` en `Session.client_id` vormen daarna harde affinity. Terminalinput, ACP, changes, accept/push en cleanup wachten bij een onderbreking op precies diezelfde client. Het verlopen van een heartbeat zet een Session niet terug in de queue. Daarmee kan een paar minuten offline internet geen tweede agent op dezelfde opdracht veroorzaken. De GUI toont bij de actieve fase welke client ontbreekt en hoe lang die offline is. De expliciete Retry-flow behoudt de logische Session/fasepoging, wist haar oude runtimebinding en materialiseert opnieuw via round-robin; een later terugkerende oude runner kan door de vervangen composition- en activation-ID's geen eigenaar meer worden.

Bij SIGTERM probeert de client eerst `goodbye(idle=...)` te sturen. Alleen een idle runner weet zeker dat er niets lokaals leeft. Een onverwachte socket-close, harde containerkill of een niet-idle goodbye wordt nooit vertaald naar automatische failover. Een admin kan een runner expliciet `drain` zetten: bestaande affinity blijft bruikbaar, maar de runner verdwijnt duurzaam uit nieuwe round-robinplaatsing en blijft ook na reconnect drained totdat hij wordt hervat. Drain doodt of verplaatst nooit zelfstandig werk; Retry blijft de bewuste failoverbeslissing.

Dockerimages zijn lokaal aan een daemon. Een nieuwe Recording met een parent blijft daarom op een runner waar die parent aanwezig is. Een nieuwe Session mag wél round-robin landen: ontbrekende immutable snapshots worden als opaque tarstream rechtstreeks van een bekende bronrunner via de server naar de doelrunner gekopieerd. De server materialiseert of inspecteert de inhoud niet. `replica_client_ids` worden duurzaam bijgehouden; delete probeert zowel primary als iedere bekende replica te wissen. Een offline bron of replica blokkeert de betreffende handeling eerlijk. Een OCI-registry kan later dezelfde export/importgrens implementeren zonder de Artifactresolver te wijzigen.

Git- en MCP-secrets blijven serverstate. Ze worden pas voor de gekozen actie ontsleuteld en reizen uitsluitend vluchtig over WSS naar de gepinde runner. Jobbijlagen worden per bestand begrensd over hetzelfde kanaal gekopieerd en daarna buiten `/workspace` geïnjecteerd. Daarom is TLS buiten localhost verplicht en valt iedere toegelaten runner plus diens Dockerdaemon binnen de trust boundary.

## 11. Security

Een toolcredential-Artifact, zoals `credential:codex`, is een image met secrets en moet als secret worden behandeld:

- user-scoped resolutie is standaard;
- geen impliciete delegatie tussen gebruikers;
- Docker images en de daemon zijn onderdeel van de trust boundary;
- de centrale snapshot-BLOB en iedere volledige backup die credentialimages bevat zijn zelf credentialmateriaal;
- alleen een afgeronde `RECORD … END RECORD`-snapshot wordt centraal opgeslagen; compositions, Session-worktrees en runtime-delta's zijn vervangbare runnercache en Retry bouwt ze opnieuw op;
- logs en metadata mogen nooit credentialinhoud tonen;
- revocation en garbage collection zijn vereist voor productie.

Git-auth volgt een smallere grens: OAuth-/toegangstokens zijn user-scoped serverstate, worden geredigeerd uit de publieke API en alleen vluchtig aan de checkouthelper aangeboden. Git access/refresh tokens, MCP env/headerwaarden en OAuth client secrets worden met AES-256-GCM en contextgebonden associated data opgeslagen. De live masterkey staat buiten `spin.db`; bij Compose in het afzonderlijke `spin-keys`-volume. Legacy JSON en losse attachments worden bij de eerste succesvolle open in SQLite geïmporteerd. Een expliciete admin-backup is één zelfstandige database en bevat daarom een portable key; die backup is secretmateriaal. Zonder de juiste bestaande live key weigert de server te starten en blijft de state onaangeraakt.

Lokale passwords zijn PBKDF2-HMAC-SHA256 hashes met unieke salts; login-sessions bewaren uitsluitend hashes van de random cookie en CSRF-token. De browsercookie is HttpOnly, SameSite=Lax en onder HTTPS `Secure` met een `__Host-` naam. Mutaties vereisen daarnaast de expliciete CSRF-header. Headless workers delen geen browserlogin: zij gebruiken een random bearer-token uit een apart volume dat geen masterkey bevat.

Het huidige systeem is daarmee een bruikbare authenticated workbench met een gedistribueerde runnerfleet, geen volledig gehard multi-tenant platform. Buiten localhost zijn TLS, gecontroleerde runnerregistratie en Docker-daemontoegang, keyback-ups, credentialrevocation en garbage collection nog operationele vereisten.

## 12. Huidige API en invarianten

Belangrijkste routes:

```text
GET  /api/state
GET  /api/auth/status
POST /api/auth/setup
POST /api/auth/login
POST /api/auth/logout
POST /api/auth/users
POST /api/auth/users/{id}/archive
POST /api/auth/users/{id}/restore
POST /api/commands
DELETE /api/artifacts/{id}
POST /api/recordings
POST /api/recordings/{id}/commands
POST /api/recordings/{id}/end
POST /api/use
POST /api/compositions/{id}/acp/probe
POST /api/compositions/{id}/stop
POST /api/jobs
POST /api/jobs/{id}/sessions
GET  /api/sessions/{id}/acp        (WebSocket)
GET  /api/runner/ws                 (runner WebSocket + bearer-token)
POST /api/clients/{id}/drain
POST /api/clients/{id}/resume
GET  /api/sessions/{id}/changes
POST /api/sessions/{id}/retry
POST /api/mcp-servers
DELETE /api/mcp-servers/{id}
POST /api/git/accounts
DELETE /api/git/accounts/{id}
POST /api/git/repositories
PUT /api/git/repositories/{id}
PUT /api/git/oauth/{provider}/configuration
DELETE /api/git/oauth/{provider}/configuration
GET /api/git/oauth/{provider}/start
GET /api/git/oauth/{provider}/callback
DELETE /api/git/repositories/{id}
POST /api/sessions/{id}/fork
POST /api/sessions/{id}/result
```

De changes-route geeft naast status en line counts ook een begrensde unified Git-patch per tekstbestand terug. De client gebruikt dezelfde patch voor een responsive reviewweergave: side-by-side wanneer er ruimte is en unified rood/groen op smalle schermen. ACP tool-call diffs en locations zijn navigatiehints naar deze worktree-gebaseerde review; ze vervangen Git niet als bron van waarheid.

Een snapshot kan launchconfiguratie voor iedere opaque capability leveren in `/etc/spin/enabled/<naam>.env`. De Docker-engine source't en exporteert dit bestand vlak voor het bijbehorende entrypoint; hij interpreteert de variabelen niet. Daardoor kan bijvoorbeeld `config:codex-network` de ACP-launch aanpassen zonder credentialdata te bevatten, terwijl hetzelfde contract bruikbaar blijft voor andere ACP-wrappers en toekomstige `ENABLED` hooks.

Invarianten:

- snapshots zijn immutable;
- remove verwijdert alleen een eigen leaf-snapshot die niet door een open opname of draaiende Composition wordt gebruikt;
- iedere parent-edge is expliciet;
- `USE kind:name` is de enige generieke selectieregel;
- user-scope wordt altijd tegen de actuele operator opgelost;
- een Composition bevat één entry-Artifact en zijn volledige closure;
- `WITH` voegt extra expliciete selectors toe zonder de entry/agent te veranderen;
- de Docker-engine gebruikt een bestaande lineaire snapshotclosure waar mogelijk en bouwt anders een tijdelijke composition-union in selectievolgorde;
- `enabled` wordt uitsluitend uit die closure opgebouwd;
- onbekende Artifacttypes en capabilities blijven bruikbaar als opaque metadata;
- iedere Job heeft precies één Git repository en iedere Session erft die binding;
- een vervolg-Job begint vanaf de remote branch van een gesloten bron-Job en maakt die bron zolang de verwijzing bestaat niet verwijderbaar;
- iedere Session-environment bevat verplicht `tool:git` in haar Artifactclosure;
- een Job heeft één `jobs/<slug-id>/main`-doelbranch en iedere Session één siblingref onder `jobs/<slug-id>/sessions/`;
- Git-accounts worden per operator laat gebonden; tokens zijn geen layers en worden alleen via stdin aan de checkouthelper blootgesteld;
- een spawnende Session moet bij dezelfde Job horen;
- iedere workflowfase eindigt uitsluitend in ACCEPT of REJECT; REJECT bevat altijd een reden;
- een menselijke workflowbeslissing bewaart expliciet zowel de ACCEPT- als REJECT-doelstaat en CHAT keurt niets impliciet goed;
- alleen de control plane mag een geaccepteerde Session committen en naar de Job-branch publiceren;
- een fase zonder `allow_changes` kan niet worden geaccepteerd wanneer haar repositorytree of HEAD van de Session-base afwijkt;
- MCP-definities zijn user-scoped en secretwaarden zijn geredigeerd in publieke responses;
- Docker-images claimen nooit process- of providercachecontinuïteit.
- alleen nieuwe workloads worden round-robin geplaatst; een bestaande Runtime en Session behouden altijd hun `client_id`;
- een drained runner behoudt bestaande affinity maar ontvangt geen nieuwe round-robinplaatsing totdat een admin hem hervat;
- een disconnect of verlopen heartbeat veroorzaakt nooit impliciete failover;
- snapshots hebben één primaire runner en nul of meer bekende replica's; opaque replicatie verandert hun digest of graphidentiteit niet;
- alleen runners bezitten de Docker-engine; de servercontainer heeft geen Docker-socket.

Dat is de truc: Spin kent de inhoud niet, maar kent genoeg structuur om iedere opgenomen tool veilig, overdraagbaar en uitbreidbaar te starten.
