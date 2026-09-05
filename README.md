> [!CAUTION]
> # DEPRECATED — DO NOT USE
> Spin is niet onderhouden, niet ondersteund en niet bedoeld voor productiegebruik. Deze repository is uitsluitend openbaar als gearchiveerd referentiemateriaal. Er wordt geen toestemming gegeven om de code te gebruiken, uit te voeren, kopiëren, wijzigen of distribueren. Zie [LICENSE](LICENSE).

# EasyACP / Spin

Tool-onafhankelijke Docker-snapshots voor overdraagbare agentomgevingen. Spin kent niet het interne login- of sessieformaat van een tool: je bouwt een expliciete snapshotketen en kiest later één logisch Artifact als entrypoint. De webserver is de control plane; één of meer uitgaande WebSocket-runners leveren de Docker-compute.

## Snel starten

Vereist: Go 1.26 en een bereikbare Docker-daemon.

```sh
go run ./cmd/spin-server
```

Open `http://127.0.0.1:8080`. De eerste browser maakt de lokale owner (username, naam en wachtwoord); daarna is iedere API-actie aan de ingelogde serveridentiteit gebonden. De Docker-engine gebruikt standaard `alpine:3.24` als onzichtbaar substraat.

De GUI heeft vier rustige werkvlakken:

- **Jobs**: maak Templates uit gewone invoervelden en start een Job als automatische reeks ACP-Sessions;
- **Environments**: beheer alle globale en user-scoped lagen en open alleen tijdens opname de fullscreen Capsule recorder;
- **Connections**: beheer Git-remotes/accounts en MCP in twee subtabs;
- **Access**: laat gebruikers zien en laat admins lokale users maken, archiveren en herstellen.

## Eerste Codex + ACP-keten

Voer in de Capsule terminal uit:

```text
RECORD tool:git --scope=global --enable=git
apk add --no-cache git openssh-client ca-certificates
END RECORD

RECORD tool:node --scope=global --from=tool:git
apk add --no-cache nodejs npm
END RECORD

RECORD tool:codex --scope=global --from=tool:node --enable=acp --command=codex-acp
npm install -g @openai/codex @agentclientprotocol/codex-acp
codex --version
END RECORD

RECORD tool:dotnet --scope=global --from=tool:codex
apk add --no-cache dotnet10-sdk
END RECORD

RECORD credential:codex --scope=user --from=tool:codex
```

De Capsule terminal is een echte interactieve PTY. Voor Codex op een headless Docker-host start je device-auth daarom rechtstreeks in de webterminal:

```text
codex login --device-auth
```

De URL en eenmalige code verschijnen direct terwijl het proces nog draait. De invoer stuurt tijdens een lopend commando stdin naar dezelfde PTY en de zichtbare `Ctrl-C`-knop onderbreekt het proces. Controleer na de login met `codex login status` en geef daarna `END RECORD`.

Spin-commando's blijven gewone HTTP-opdrachten aan de orchestrator. Iedere andere regel binnen een actieve opname start via WebSocket een `docker exec -it` met een echte pseudo-terminal; output wordt uitsluitend live gestreamd en na exit blijft alleen minimale executionmetadata over.

Een Capsule ondersteunt maximaal acht gelijktijdige PTY-kanalen. `+ PTY` maakt de invoer vrij voor een nieuw proces in dezelfde container; kies daarna een kanaalchip om weer naar diens stdin en Ctrl-C terug te gaan. Dit is onder meer bruikbaar wanneer een loginserver op container-localhost wacht: laat de login in het ene kanaal staan en roep de callback met `wget` of `curl` aan vanuit een tweede kanaal.

Spin bewaart voor geen enkele Recording commandtekst of transcript. Alleen volgnummer, exitcode en tijdstip vormen een minimaal execution ledger; live output bestaat uitsluitend in de browserstream. Bestaande historische input/output wordt bij het openen van de state permanent verwijderd.

Start daarna de omgeving en verifieer de echte ACP-entrypoint:

```text
USE credential:codex
ACP PROBE
```

Meerdere opgenomen onderdelen selecteren kan met `WITH`:

```text
USE tool:codex WITH tool:dotnet
```

De eerste selector blijft de agent/entry; extra selectors zijn buildtools, configuratie of andere lagen. Environments bevat hiervoor een directe stack-launcher. Voor automatische Jobs is de verantwoordelijkheid bewust verdeeld: het Template kiest een environment die `git` enablet, de Repository kiest projectlagen zoals Node of .NET, en de Job kiest een environment die `acp` enablet. Bij het inschieten bevriest Spin die drie delen als één Session-recipe.

Als één gekozen snapshot alle andere in zijn parentclosure bevat, start Docker die image direct. Bij onafhankelijke branches bouwt Spin vluchtig één composition-image: de minimaal benodigde snapshots worden in de gekozen `USE`/`WITH`-volgorde als filesystem-union gestreamd, zonder tussentijdse export op disk. Latere snapshots winnen bij bestandsconflicten; hun afwezigheid verwijdert geen bestand uit een eerdere onafhankelijke branch. Gebruik `--from` wanneer exacte delete-semantiek of een vaste lineage nodig is. De tijdelijke composition-image verdwijnt weer bij `STOP`.

`USE credential:codex` kiest bij operator Derek zijn snapshot en bij John die van John. De parentketen wordt automatisch meegenomen. `USE tool:codex` start bewust dezelfde tool zonder credentiallaag.

Voeg onder Connections → Git een remote zonder ingebedde secrets toe, selecteer de projectlagen en kies credentialscope `user` of `global`. Git-auth is nadrukkelijk geen snapshotlaag en een repository bewaart geen account-ID. De Job-wizard maakt automatisch `Job → root Session → Composition`; checkout, push en PR resolven op dat moment de provideridentity via remote-host, scope en uitvoerende gebruiker. Nieuwe Jobs worden geweigerd wanneer de uiteindelijke Composition niet zowel `git` als `acp` enablet; artifactnamen spelen daarbij geen rol. Alleen oude ongekoppelde repositories blijven als `public` migratievorm leesbaar totdat je er een identityscope voor kiest.

Een repository kan via één edit-dialog van naam, remote-URL, default branch, credentialscope en projectlagen veranderen. Een Job bevriest bij creatie repositorynaam, remote, provider, scope en base branch; edits gelden dus voor nieuwe Jobs en kunnen een lopende review of PR nooit stilletjes naar een andere remote verplaatsen.

Een Job krijgt `jobs/<naam-id>/main` als remote bron van waarheid. Vóór de eerste agent start maakt de kortlevende Git-helper die ref vanaf de gekozen basisbranch en leest hem opnieuw terug; iedere Session begint daarna vanaf die remote Job-head. Iedere Session krijgt een echte lokale `jobs/<naam-id>/sessions/<session-id>` in haar geïsoleerde workspace en wijst terug naar de Job-branch als reviewdoel. De namen zijn siblings omdat Git geen branch en een childref onder exact diezelfde branchnaam toestaat. Vanuit dezelfde Job-kaart kan een mens een extra Session starten en met `spawned_by_session_id` kan een Session/agent dat via de API ook zelf doen. Review/promotie naar de Job-branch blijft een orchestratoractie; agents ontvangen geen remote credential.

Iedere Templateflow eindigt verplicht met een door Spin toegevoegde `Pull request`-systeemstap. De control plane maakt de PR van `jobs/<naam-id>/main` naar de oorspronkelijke Job-basisbranch op de repository van die Job. De Job-naam wordt de PR-titel en de volledige Goal de PR-omschrijving. Providercredentials blijven server-side; de eerste adapter ondersteunt GitHub. Zonder geslaagde PR blijft de Job pending en kan de gebruiker de PR-actie opnieuw proberen—`KLAAR` betekent dus dat er daadwerkelijk een PR bestaat.

## Templates en automatische Jobs

Een Template is een tabel van fasen, geen ingebouwd type zoals “ontwikkeling” of “bugfix”. Het Template kiest tevens de generieke Git-enablement. Per fase leg je de opdracht, eventuele Markdown-deliverables, expliciet te injecteren eerdere deliverables, `mag repository wijzigen`, de `ACCEPT`-route, de `REJECT`-route en een rejectlimiet vast. De ACCEPT- en REJECT-route hebben ieder een eigen optionele `ASK USER`-gate. Een Job combineert zo'n Template met alleen een naam, goal, repository en ACP-environment. Spin maakt per fase een nieuwe geïsoleerde Session en start de ACP-agent automatisch.

`Job inschieten` bewaart de Job en eerste queued Session direct en retourneert vóór Git, Docker en ACP worden gestart. De browser blokkeert dubbel submitten; een duurzame, user-gebonden idempotency-key zorgt daarnaast dat retries of een klikburst server-side dezelfde Job teruggeven. De zware start gebeurt daarna op de achtergrond.

Het kruisje op een Job annuleert een eventuele achtergrondstart, stopt zijn lokale Session-capsules en verwijdert alle bijbehorende lokale workflowstate. De Git-repository en remote Job/Session-branches blijven bewust bestaan.

Een gesloten Job kan door iedere ingelogde collega als vervolg worden geforkt. De nieuwe Job wordt van die user, krijgt een eigen branch en workflow, en gebruikt diens laat gebonden Git-identity, maar begint verplicht vanaf de remote resultaatbranch van de bron-Job. De oorspronkelijke goal, laatste revisie van ieder deliverable en alle oorspronkelijke PDF-/afbeeldingsbijlagen gaan als immutable ACP-context mee. Daardoor kan feedback of een nagekomen bug worden opgepakt zonder de oude Job opnieuw te openen. Zolang een vervolg ernaar verwijst, kan de bron-Job niet worden verwijderd.

Iedere workflow-Session krijgt via een intern, kortlevend MCP-kanaal dezelfde kleine set acties:

- `ask(question)` pauzeert voor input;
- `add_deliverable(name, content)` bewaart een benoemde Markdown-bijlage;
- `accept(summary)` en `reject(reason)` sluiten de AI-uitkomst af; reject vereist altijd een reden.

Er bestaat bewust geen committool voor de agent. Bij definitieve `ACCEPT` controleert Spin in dezelfde draaiende Session de worktree. In een schrijffase worden dirty files en eventuele agentcommits tot één herkenbare workflowcommit vanaf de oorspronkelijke Session-base gevouwen. Daarna pusht alleen de control plane fast-forward `HEAD` naar `jobs/<naam-id>/main` en verifieert met een nieuwe remote lookup dat die ref exact dezelfde commit aanwijst. Pas daarna kan de volgende fase starten. Een fase zonder schrijfrecht neemt bij ACCEPT niets mee: de agent mag in zijn wegwerp-workspace restoren, bouwen en experimenteren, maar Spin commit niets en bevestigt alleen de onveranderde Session-basis op de remote Job-ref. Remote credentials bereiken de agent nooit.

De Job Goal wordt altijd geïnjecteerd. Een fase ontvangt daarnaast uitsluitend de deliverable-namen die het Template voor die fase aanvinkt, telkens als volledige laatste revisie. Een geselecteerd document is verplichte context: zonder bestaande revisie start de overgang niet.

`ASK USER` is per ACCEPT- of REJECT-route een vinkje/gate, geen AI-uitkomst. Zo kan ACCEPT menselijke goedkeuring vragen terwijl REJECT nog automatisch terugloopt. Na bijvoorbeeld `AI ACCEPTED` ziet de gebruiker de vaste routes en kiest die `ACCEPT`, `REJECT` met een eigen reden, of `CHAT`. Bij `REJECT` gaat een nieuwe Session over de geconfigureerde terug-route. Na het ingestelde aantal automatische rejections wordt dezelfde gate getoond, inclusief de laatste reden. `CHAT` opent dezelfde ACP-Session; pas bij het sturen van een bericht wordt die hervat, zonder impliciete goed- of afkeuring. De wachtende beslissing blijft staan: zolang de agent werkt zijn de knoppen dicht, en zodra de beurt eindigt zonder nieuw besluit zijn `ACCEPT` en `REJECT` weer klikbaar.

Deliverables staan als bijlagen bij zowel de fase als de chat en openen als volledig Markdown-document. Bovenin kan tussen alle immutable revisies worden gewisseld. Alleen op de laatste revisie kan een ingelogde gebruiker tekst selecteren en een permanente comment plaatsen; historische revisies en hun bestaande comments zijn read-only. De server controleert bij iedere comment opnieuw of de revisie nog actueel is, zodat een oude browsertab geen retroactieve feedback kan toevoegen. Comments op de laatste revisies gaan als onafhankelijke context naar iedere nieuwe workflow-Session en staan los van ACCEPT/REJECT.

Code review volgt hetzelfde immutable model. De algemene `Changes`-knop op een Job opent altijd de volledige boom vanaf de basisbranch; de knop bij een fase opent uitsluitend de laatste diff van die poging. Het openen van deze grote reviewweergave legt precies één dedupliceerde revisie vast. Selecteer tekst of klik een coderegel om een permanente comment met bestand, zijde en regelbereik te plaatsen. Oudere diffrevisies blijven via de revisiebalk terugleesbaar maar kunnen niet achteraf worden aangepast. Wanneer de bijbehorende workflowpoging wordt gereject, injecteert Spin zowel de rejectreden als alle codecomments in de volgende Session; diens fasediff begint opnieuw klein terwijl de volledige Job-boom bovenin beschikbaar blijft.

De Job toont `BEZIG`, `PENDING · ASK`, `PENDING · USER` of `KLAAR`, plus alle pogingen. Zo blijft de flow generiek terwijl Templateconfiguratie bepaalt waar iedere beslissing heen gaat.

## Het model

```text
Alpine engine-substraat
└── tool:git                   ENABLES git
    └── tool:node
        └── tool:codex             ENABLES acp
            ├── credential:codex   scope=user:derek
            └── credential:codex   scope=user:john
```

- `kind:name` is overal de selectorvorm.
- `--from=kind:name` maakt een parent expliciet voor ieder soort laag.
- `scope=user` bewaart dezelfde logische selector afzonderlijk per operator, dus ook bijvoorbeeld `tool:git` of `tool:dotnet`.
- `USE kind:name` materialiseert het gekozen Artifact plus zijn volledige parentclosure.
- `ENABLED` is geërfde capabilitymetadata. `git` markeert de control-plane Git-runtime; `acp` publiceert daarnaast de agent-entrypoint. Toekomstige namen blijven opaque totdat een hook ze interpreteert.

Persoonlijke MCP-definities staan los van de Dockerlaag. Ze volgen de ACP `session/new`-vorm (`command`/`args`/`env` voor stdio of `url`/`headers` voor HTTP), worden user-scoped geselecteerd en reizen als IDs mee van Job naar Session en Composition. De publieke state bevat alleen geredigeerde secretvelden. Wanneer je bij een draaiende Job-Session `Open chat` kiest, geeft de server de private waarden rechtstreeks aan `session/new`; ze komen niet in de browser.

Git-toegang gebruikt app-managed `GitAccount`-objecten. Een user-scoped repository kiest automatisch het account van de uitvoerende gebruiker voor de remote-host; global kiest het gedeelde service-account voor die host. De Docker-engine start voor checkout een kortlevende helper vanaf de gekozen Git-capabele environment. Het accounttoken gaat via stdin naar Git's vluchtige shell `credential.helper`; er wordt geen credentialbestand gemaakt en het geheim staat niet in Docker-argumenten, containerconfig, de Composition of de Session-image. Alleen het workspace-volume gaat daarna naar de agent. Zo maakt een door John uitgevoerde GitHub-actie ook Johns PR, zonder repositorybinding aan degene die de remote ooit toevoegde.

Voor echte provider-login open je als admin Connections → Git. Spin toont voor GitHub en GitLab de exacte callback-URL en een link naar de provider waar je de OAuth application maakt. Plak daarna Client ID en Client secret in Spin; het secret wordt versleuteld opgeslagen. `Koppel GitHub/GitLab` verschijnt zodra de provider klaarstaat.

Environmentconfiguratie blijft beschikbaar voor beheerde deployments en heeft voorrang op appconfiguratie:

```sh
export SPIN_PUBLIC_URL=http://127.0.0.1:8080
export SPIN_GITHUB_CLIENT_ID=...
export SPIN_GITHUB_CLIENT_SECRET=...
export SPIN_GITLAB_CLIENT_ID=...
export SPIN_GITLAB_CLIENT_SECRET=...
```

Registreer als callbacks respectievelijk `${SPIN_PUBLIC_URL}/api/git/oauth/github/callback` en `${SPIN_PUBLIC_URL}/api/git/oauth/gitlab/callback`. Zonder `SPIN_PUBLIC_URL` leidt Spin de callback af van de host waarop je de GUI opent. De flow gebruikt authorization code + PKCE, haalt de provideridentiteit op en bewaart die bij de ingelogde user. Voor self-hosted of nog niet geconfigureerde providers blijft een handmatige HTTPS-tokenfallback beschikbaar.

De ACP-hook volgt het stabiele ACP v1-transport: newline-delimited JSON-RPC 2.0 over stdio. `ACP PROBE` blijft beschikbaar als korte diagnostische handshake. Voor een Job-Session houdt Spin het subprocess levend en doorloopt het `initialize → session/new → session/prompt`; `session/update`, plannen, tool calls en permission requests worden live naar het chatscherm gestreamd. De Changes-kolom leest de echte Git-workspace en toont per bestand toegevoegde en verwijderde regels.

## Commando's

```text
RECORD kind:name [--scope=user|global] [--from=kind:name]
                 [--enable=git|acp] [--command=codex-acp]
                 [--transport=stdio] [--protocol-version=1]
<opaque shell command>
END RECORD
CANCEL RECORD
FROM kind:name
LIST [kind]
USE kind:name [WITH kind:name ...] [--profile=default]
USE session:<session-id>
ACP PROBE [composition-id]
STOP USE [composition-id]
```

De oude tweedelige vormen zoals `RECORD tool codex` en `USE tool codex` worden nog gelezen als compatibiliteit, maar de GUI en documentatie schrijven alleen de canonieke selectorvorm.

## Server, client en opslag

- `cmd/spin-server`: HTTP-server, web-GUI, state, Git/OAuth en orchestrator; deze container heeft geen Docker-socket.
- `cmd/spin-client`: reconnectende Docker-runner voor snapshots, Git-workspaces, PTY, ACP en reviewoperaties.
- `internal/capsule`: journalengine en echte Docker commit/clone-engine.
- `internal/store`: persistente Artifactgraph, scope-resolutie, Jobs en Sessions.
- `internal/server`: GUI, REST, commandparser en de langlevende ACP-session-supervisor.
- `var/spin.db`: centrale SQLite-database met control-plane-state, Job-bijlagen en de opaque exports van iedere afgeronde Docker-snapshot.
- `var/spin-state.json` en `var/job-attachments`: alleen eenmalige legacy-importbronnen; na import is `spin.db` leidend.
- `var/spin-master.key`: lokale AES-masterkey; apart van de state back-uppen en nooit publiceren.
- `var/spin-worker.token`: apart bearer-token voor het headless runner-WebSocket.
- `var/spin-client.id`: stabiele runneridentiteit; blijft gelijk over containerrestarts.

Nieuwe workloads worden round-robin over online, niet-drainende runners verdeeld. Een admin kan een runner handmatig drainen: bestaand werk en zijn vaste affinity blijven bereikbaar, maar nieuw werk slaat hem over totdat hij wordt hervat. Zodra een Recording of Session een runner heeft, bewaart zijn runtime die `client_id`: een kort netwerkverlies verandert nooit de uitvoerder. Zowel server als client sturen WebSocket Ping-frames, eisen tijdige Pong-frames en vervangen een verbroken socket met exponential backoff. Pending RPC's gebruiken stabiele request-ID's en worden na reconnect idempotent hervat; ACP- en PTY-streams blijven aan dezelfde logische client gekoppeld.

Een nette SIGTERM stuurt best-effort `goodbye` met de lokale idle-status. Een harde Docker-kill of ontbrekend internet is nadrukkelijk geen bewijs dat de workload dood is en veroorzaakt dus geen automatische failover. De Job toont bij de actieve fase welke client ontbreekt en hoe lang die offline is. `Retry` behoudt de logische fasepoging, verbreekt bewust de oude runtime-affinity en materialiseert een nieuwe Capsule via round-robin. Een later terugkerende oude runner kan die opnieuw gekoppelde Session niet meer overnemen.

Docker-images zijn runner-lokale caches, niet langer de bron van waarheid. Bij `END RECORD` streamt de runner eerst een opaque `docker image save` naar gechunkte BLOB-rows in `spin.db`; pas na die duurzame archivering wordt het Artifact afgerond. Wanneer een nieuwe Session op een andere runner landt, gebruikt Spin een online replica of laadt de centrale export terug en onthoudt de nieuwe cachekopie. Layerinhoud wordt niet geïnterpreteerd en een verdwenen laptop vernietigt dus geen Artifact.

Die opslaggrens is bewust hard: uitsluitend afgeronde `RECORD … END RECORD`-lagen worden centrale artifacts. Tijdelijke composities, containers, Session-worktrees en hun Docker-delta's blijven wegwerpcache op de runner. Retry begint opnieuw bij de opgeslagen artifacts en de actuele remote Job-branch; er hoeft geen half afgemaakte runtime te worden verhuisd.

Access → Backup downloadt één consistente `spin-backup-<tijd>.db` met state, portable masterkey, bijlagen en alle vereiste Docker-snapshots. Restore opent de upload eerst apart, ontsleutelt en valideert de state, leest iedere BLOB volledig terug met SHA-256, maakt een lokaal rollbackpunt en vervangt daarna pas de actieve database. Zo'n backup bevat zowel credentials als credential-images en moet als een passwordbestand worden behandeld.

Bij de eerste bestaande-state-start worden oude plaintext Git/MCP/OAuth-secretwaarden automatisch herschreven als AES-256-GCM-enveloppen. Starten met een ontbrekende of verkeerde bestaande masterkey stopt met een expliciete fout; Spin overschrijft de state dan niet.

Belangrijke extra routes:

```text
GET    /api/auth/status
POST   /api/auth/setup
POST   /api/auth/login
POST   /api/auth/logout
POST   /api/auth/users
POST   /api/auth/users/{id}/archive
POST   /api/auth/users/{id}/restore
DELETE /api/artifacts/{id}
POST   /api/jobs/{job-id}/sessions
DELETE /api/jobs/{job-id}
POST   /api/mcp-servers
DELETE /api/mcp-servers/{id}
POST   /api/git/accounts
DELETE /api/git/accounts/{id}
POST   /api/git/repositories
PUT    /api/git/repositories/{id}
PUT    /api/git/oauth/{provider}/configuration
DELETE /api/git/oauth/{provider}/configuration
GET    /api/git/oauth/{provider}/start
GET    /api/git/oauth/{provider}/callback
DELETE /api/git/repositories/{id}
GET    /api/sessions/{id}/acp          (WebSocket)
GET    /api/runner/ws                   (runner WebSocket + bearer-token)
POST   /api/clients/{id}/drain
POST   /api/clients/{id}/resume
POST   /api/sessions/{id}/retry
GET    /api/sessions/{id}/changes
POST   /api/jobs/{job-id}/code-reviews
GET    /api/code-reviews/{revision-id}
POST   /api/code-reviews/{revision-id}/comments
POST   /api/workflow-templates
DELETE /api/workflow-templates/{id}
POST   /api/workflow/questions/{id}/answer
POST   /api/workflow/mcp/{session-id} (intern Streamable HTTP MCP)
```

De Session-chat rendert ACP message-, thought-, plan-, permission- en tool-updates als aparte compacte onderdelen. `changes` bevat daarnaast begrensde Git-patches per bestand; de browser toont die naast elkaar op brede schermen en als één rood/groen spoor op smalle schermen. Git blijft daarmee de reviewwaarheid, ook wanneer een agent zijn ACP-update onvolledig invult.

Browsermutaties vereisen de HttpOnly login-cookie plus de per-session `X-Spin-CSRF` header; `operator`/`actor` uit body of query wordt genegeerd en server-side uit de login bepaald. Runners gebruiken niet de browsercookie maar hun afzonderlijke bearer-token. Gebruik buiten localhost altijd `https://`/`wss://`; Git-, MCP- en snapshotcredentialmateriaal reist vluchtig over dit kanaal.

Snapshot-remove is bewust streng: alleen de maker kan verwijderen, en alleen als de snapshot geen child, open opname of draaiende Composition voedt. Bij Docker wordt ook de onderliggende image verwijderd.

Een laag kan de startomgeving van een capability aanvullen via `/etc/spin/enabled/<capability>.env`. Spin exporteert de variabelen uit dit bestand alleen wanneer die `ENABLED` entrypoint start. Dit houdt runtimebeleid stapelbaar en toolonafhankelijk; secrets horen nog steeds in user-scoped credentiallagen en niet in een globale configlaag.

De Session-container heeft gewoon netwerk (`-capsule-network bridge`), maar de sandbox van Codex zelf staat in de standaardmodus `agent` (workspace-write) zonder netwerktoegang. Een `dotnet restore` of `npm install` strandt dan op de packagebronnen, ook al kan de container ze bereiken. codex-acp leest `CODEX_CONFIG`, een JSON-object dat in de Codex-sessieconfig wordt gemerged; zet het netwerk aan met een configlaag die alleen dit bestand levert:

```text
RECORD config:codex-network --scope=global --from=tool:codex
mkdir -p /etc/spin/enabled
printf '%s\n' 'CODEX_CONFIG={"sandbox_workspace_write":{"network_access":true}}' > /etc/spin/enabled/acp.env
END RECORD
```

Voeg `config:codex-network` toe aan de toolinglagen van de repository of aan de `WITH`-lagen van een fase. Spin interpreteert de variabele niet; dezelfde haak draagt ook `INITIAL_AGENT_MODE` (`read-only`, `agent`, `agent-full-access`) of een andere ACP-wrapper.

Bij `session/new` geeft Spin zowel `/workspace` als de capsule-HOME `/root` door. ACP-agents die `additionalDirectories` ondersteunen nemen HOME daardoor op als writable root van hun workspace-sandbox. Gewone tooling kan dus zonder productspecifieke uitzonderingen naar bijvoorbeeld `/root/.dotnet`, `/root/.npm` of `/root/.cache` schrijven. Dit is uitsluitend `/root` ín de geïsoleerde, gematerialiseerde Session-container; de host en de immutable bronsnapshot worden niet schrijfbaar. Een laag kan `/etc/spin/enabled/acp.env` nog steeds gebruiken voor aanvullende runtimeconfiguratie zoals netwerkbeleid.

Alternatieve start:

```sh
docker compose up --build
```

Kies een vrije hostpoort met `SPIN_PORT=8090 docker compose up --build`. De hostbinding is veilig standaard `127.0.0.1`; alleen als een reverse proxy of netwerkdeployment dat bewust vereist verander je die, bijvoorbeeld met `SPIN_BIND=0.0.0.0`. Compose houdt state, keys, runnerauth en de stabiele client-ID in aparte volumes. Alleen `spin-client` mount `/var/run/docker.sock`; de server kan geen container starten. De runner leest het workertoken, maar krijgt nooit toegang tot de masterkey of serverstate.

Een extra laptop/server draait dezelfde clientimage met een eigen persistent ID en hetzelfde servertoken:

```sh
docker build --target client -t easyacp-client .
docker run --restart unless-stopped \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v spin-client-data:/client-data \
  -e SPIN_WORKER_TOKEN='<server worker token>' \
  easyacp-client \
  -server https://spin.example.test -name laptop-john \
  -id-file /client-data/client.id -capsule-network bridge
```

Connections → Runners toont online/offline/draining, engine, capaciteit, last-seen en hoeveel Sessions duurzaam aan iedere client hangen. Admins kunnen daar nieuwe plaatsing per runner drainen en hervatten.

### Releases

`release.sh` bouwt één matrix en publiceert die zowel onder een immutable versie als onder de overschrijfbare `rolling`-release:

```sh
./release.sh v1.0.0
```

De Linux-server en -client worden statisch gebouwd voor amd64 en arm64. De client draait ook in Docker nog steeds als native binary van de hostarchitectuur; Docker levert de verpakking, Docker CLI en socket, geen CPU-emulatie. Daarnaast bouwt het script echte HopOS/Tamago server-ELF's voor arm64 en riscv64 tegen `${HOPOS_DIR:-$HOME/Git/hop-os}`. Met `PUBLISH=0` voer je alleen de volledige compile-gate uit.

Een client krijgt alleen de publieke server-URL. `https://spin.example.test` wordt automatisch `wss://spin.example.test/api/runner/ws`; bij disconnect gebruikt hij ping/pong en exponential reconnect. Server en client delen uitsluitend `SPIN_WORKER_TOKEN`.

De HopOS-server verwacht een gepubliceerde `ER_PORT_HTTP`, een gemounte `/data` voor `/data/spin.db`, `SPIN_WORKER_TOKEN` en bij voorkeur een vaste base64 `SPIN_MASTER_KEY`. SQLite v0.35.4 is libc-vrij via wasm2go; een eigen HopOS-VFS vertaalt random-access pagina-I/O naar de volume-ABI. De echte releasegate bouwt dit pad voor arm64 én riscv64. Control plane, runner-WebSocket, Job-bijlagen, centrale Docker-snapshots en databasebackup zijn daardoor op beide targets beschikbaar.

Bij runners buiten het lokale Compose-netwerk moet `SPIN_INTERNAL_URL` op de server een voor de agentcontainers bereikbare HTTPS-URL zijn (meestal dezelfde reverse-proxy-URL als `SPIN_PUBLIC_URL`). De standaard `http://server:8080` is alleen geldig voor de meegeleverde lokale Compose-runner; workflow-MCP gebruikt deze URL vanuit de Session-container.

## Verifiëren

```sh
GOCACHE=/tmp/easyacp-go-cache go test -race ./...
GOCACHE=/tmp/easyacp-go-cache go vet ./...
GOCACHE=/tmp/easyacp-go-cache go build ./cmd/spin-server ./cmd/spin-client
```

## Belangrijke grens

Een Docker image commit bewaart filesystemstate, geen RAM, open sockets of provider-side KV/prompt cache. Tool-loginimages zoals `credential:codex` bevatten echte secrets en moeten daarom als secretmateriaal worden behandeld. Git-, MCP- en OAuth-secrets staan AES-256-GCM-versleuteld in de server-state en worden nooit door de state-API teruggestuurd; de live masterkey staat bewust in een apart bestand/volume. Alleen een expliciete admin-backup voegt een portable kopie van die key aan de gedownloade database toe, zodat één bestand daadwerkelijk herstelbaar is.

Spin heeft nu lokale app-authenticatie, user-scoped zichtbaarheid, CSRF-bescherming en een apart runnerkanaal. Voor toegang buiten localhost blijft TLS via een vertrouwde reverse proxy vereist. Iedere toegelaten runner en diens Docker-daemon vallen binnen de trust boundary; credentialimages kunnen naar de runner van de betreffende workload worden gerepliceerd. Tokenrotatie/revocation, per-runner attestatie en een externe secret manager zijn logische vervolgstappen voor een echte multi-tenant deployment.

Zie [design.md](design.md) voor de resolverinvarianten, ACP-lifecycle en het Job/Session/forkmodel.
