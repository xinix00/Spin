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
- **Environments**: beheer alle globale en user-scoped lagen en maak een laag met **Nieuwe laag** (of **+ Laag** op een bestaande laag): een schermpje vraagt soort, naam, scope, waarop hij bouwt en wat hij ENABLES, daarna werk je in de shell van de fullscreen Capsule recorder en sluit je af met End & save of Cancel. Er is geen commandotaal; de knoppen praten met de API hieronder en de log zegt in gewone woorden wat er gebeurd is;
- **Connections**: beheer Git-remotes/accounts en MCP in twee subtabs;
- **Access**: laat gebruikers zien en laat admins lokale users maken, archiveren en herstellen.

## Eerste Codex + ACP-keten

Klik onder Environments op **Nieuwe laag**. Iedere laag is één opname: kies in het schermpje soort, naam, scope, waarop hij bouwt en wat hij ENABLES (de presets vullen dat in), klik **Opname starten**, typ in de shell van de capsule wat erin moet en klik **End & save**.

| Laag | Bouwt op | ENABLES | In de shell |
| --- | --- | --- | --- |
| `tool:git` (global) | Alpine-basis | git | `apk add --no-cache git openssh-client ca-certificates` |
| `tool:node` (global) | tool:git | | `apk add --no-cache nodejs npm` |
| `tool:codex` (global) | tool:node | acp · commando `codex-acp` | `npm install -g @openai/codex @agentclientprotocol/codex-acp` |
| `tool:dotnet` (global) | tool:codex | | `apk add --no-cache dotnet10-sdk` |
| `credential:codex` (user) | tool:codex | | `codex login --device-auth`, daarna `codex login status` |

De Capsule terminal is een echte interactieve PTY, in de browser getekend door xterm.js: toetsaanslagen gaan rauw naar het proces, kleuren, cursorbewegingen en TUI's werken, en de terminal past zich aan het venster aan. Zodra een capsule klaar is (je open opname, of je USE-compositie) opent Spin er een shell in; daar typ je direct. `+ shell` opent een tweede shell in dezelfde capsule en ieder paneel blijft na afloop staan (een login-URL blijft dus leesbaar) tot je het sluit. In een USE-compositie belandt niets in een laag: dat is de plek om rond te kijken of in te loggen. Voor Codex op een headless Docker-host start je device-auth daarom rechtstreeks in die shell: de URL en eenmalige code verschijnen terwijl het proces draait, en de `Ctrl-C`-knop onderbreekt het. Een tweede shell is handig wanneer een loginserver op container-localhost wacht: laat de login in de ene staan en roep de callback met `wget` of `curl` aan vanuit de andere. Een Capsule ondersteunt maximaal acht gelijktijdige shells.

Iedere shell is een `docker exec -it` met een echte pseudo-terminal over een WebSocket; output wordt uitsluitend live gestreamd en na exit blijft alleen minimale executionmetadata over.

Spin bewaart voor geen enkele Recording commandtekst of transcript. Alleen volgnummer, exitcode en tijdstip vormen een minimaal execution ledger; live output bestaat uitsluitend in de browserstream. Bestaande historische input/output wordt bij het openen van de state permanent verwijderd.

Start daarna de omgeving met **Start** op de laag `credential:codex` en verifieer de echte ACP-entrypoint met **ACP probe** op de compositiekaart. Een compositie is een stapel lagen op volgorde, van onder naar boven. Elke laag draagt alleen zijn eigen diff bij (wat die opname veranderde); de runner legt ze in die volgorde over elkaar. De stapel wordt zo gebouwd: iedere gekozen laag na zijn ouders, gekozen lagen in de opgegeven volgorde, en iedere laag opgetild naar zijn nieuwste bruikbare versie, direct boven de versie die hij vervangt. Zo bereikt een bewerken van `tool:codex` elke `credential:codex` die op de oude versie is opgenomen. De bovenste laag met een agent bepaalt welke agent en welk commando starten; lagen zonder agent (buildtools, configuratie) veranderen daar niets aan. De runner bouwt niet vanaf de bodem: de diepste laag waarvan de image precies de stapel tot daar is, is de basis; alleen wat erboven ligt wordt aangebracht, elk als eigen diff. Een opgeruimde oudere versie wordt gedragen door de eerstvolgende versie erboven (die gaat er in zijn geheel over); de wortel van een losse keten ook. Voor automatische Jobs is de verantwoordelijkheid verdeeld: het Template kiest een laag die `git` enablet, de Repository kiest projectlagen zoals Node of .NET, en de Job kiest de laag met de agent. Een Template-stap kan daar eigen lagen bovenop leggen: een laag zonder agent (bijvoorbeeld `tool:dotnet`) komt erbij en laat de agent van de Job staan; een laag met een agent komt erop als de credential-laag van de werker voor die agent, en is dan de bovenste agent. Bij het inschieten bevriest Spin die delen als één stapel per Session.

Als één gekozen snapshot alle andere in zijn parentclosure bevat, start Docker die image direct. Bij onafhankelijke branches bouwt Spin vluchtig één composition-image: de minimaal benodigde snapshots worden in de gekozen `USE`/`WITH`-volgorde als filesystem-union gestreamd, zonder tussentijdse export op disk. Latere snapshots winnen bij bestandsconflicten; hun afwezigheid verwijdert geen bestand uit een eerdere onafhankelijke branch. Laat een laag op een andere bouwen ("bouwt op" in het formulier) wanneer exacte delete-semantiek of een vaste lineage nodig is. De tijdelijke composition-image verdwijnt weer bij **Stop**.

Start op `credential:codex` kiest bij operator Derek zijn snapshot en bij John die van John. De parentketen wordt automatisch meegenomen. Start op `tool:codex` start bewust dezelfde tool zonder credentiallaag.

Voeg onder Connections → Git een remote zonder ingebedde secrets toe, selecteer de projectlagen en kies credentialscope `user` of `global`. Git-auth is nadrukkelijk geen snapshotlaag en een repository bewaart geen account-ID. De Job-wizard maakt automatisch `Job → root Session → Composition`; checkout, push en PR resolven op dat moment de provideridentity via remote-host, scope en uitvoerende gebruiker. Nieuwe Jobs worden geweigerd wanneer de uiteindelijke Composition niet zowel `git` als `acp` enablet; artifactnamen spelen daarbij geen rol. Alleen oude ongekoppelde repositories blijven als `public` migratievorm leesbaar totdat je er een identityscope voor kiest.

Een Job kan aan een collega worden toegewezen (Jobs → Mijn/Alle, de persoon op de kaart). Dat geldt vanaf de volgende stap: de lopende stap maakt af wie hem startte, iedere stap daarna draait als de assignee, met diens credential-lagen en Git-identiteit. Eigenaar en assignee kunnen allebei de chat openen, beslissen en een stap opnieuw proberen; verwijderen en sluiten blijft aan de eigenaar.

Een repository kan via één edit-dialog van naam, remote-URL, default branch, credentialscope en projectlagen veranderen. Een Job bevriest bij creatie repositorynaam, remote, provider, scope en base branch; edits gelden dus voor nieuwe Jobs en kunnen een lopende review of PR nooit stilletjes naar een andere remote verplaatsen.

Een Job krijgt `jobs/<naam-id>/main` als remote bron van waarheid; met een referentie (ticketnummer, bijvoorbeeld `#1234`) wordt dat `jobs/#1234/main`. Een fork erft de referentie van zijn bron en krijgt, omdat die branch al bestaat, `jobs/#1234/<naam-id>/main`; zo staan alle Jobs op hetzelfde ticket bij elkaar. Vóór de eerste agent start maakt de kortlevende Git-helper die ref vanaf de gekozen basisbranch en leest hem opnieuw terug; iedere Session begint daarna vanaf die remote Job-head. Iedere Session krijgt een echte lokale `jobs/<naam-id>/sessions/<session-id>` in haar geïsoleerde workspace en wijst terug naar de Job-branch als reviewdoel. Werk in uitvoering blijft niet in één Docker-volume: na iedere agentbeurt maakt Spin van de dirty files een WIP-commit en pusht de Session-branch naar de remote (het Changes-paneel zegt "op remote <commit> <tijd>"). Een Session die opnieuw start, op welke runner ook, gaat verder vanaf die branch. ACCEPT vouwt de WIP-commits tot de ene commit op de Job-branch en haalt de Session-branch daarna van de remote. De namen zijn siblings omdat Git geen branch en een childref onder exact diezelfde branchnaam toestaat. Vanuit dezelfde Job-kaart kan een mens een extra Session starten en met `spawned_by_session_id` kan een Session/agent dat via de API ook zelf doen. Review/promotie naar de Job-branch blijft een orchestratoractie; agents ontvangen geen remote credential.

Iedere Templateflow eindigt verplicht met een door Spin toegevoegde `Pull request`-systeemstap. De control plane maakt de PR van `jobs/<naam-id>/main` naar de oorspronkelijke Job-basisbranch op de repository van die Job. De Job-naam wordt de PR-titel en de volledige Goal de PR-omschrijving. Providercredentials blijven server-side; de eerste adapter ondersteunt GitHub. Zonder geslaagde PR blijft de Job pending en kan de gebruiker de PR-actie opnieuw proberen—`KLAAR` betekent dus dat er daadwerkelijk een PR bestaat.

## Templates en automatische Jobs

Een Template is een tabel van fasen, geen ingebouwd type zoals “ontwikkeling” of “bugfix”. Het Template kiest tevens de generieke Git-enablement. Per fase leg je de opdracht, eventuele Markdown-deliverables, expliciet te injecteren eerdere deliverables, `mag repository wijzigen`, de `ACCEPT`-route, de `REJECT`-route en een rejectlimiet vast. De ACCEPT- en REJECT-route hebben ieder een eigen optionele `ASK USER`-gate. Een Job combineert zo'n Template met alleen een naam, goal, repository en ACP-environment. Spin maakt per fase een nieuwe geïsoleerde Session en start de ACP-agent automatisch.

Een Job ligt bij iemand: bij aanmaken bij de eigenaar, daarna bij wie hem toegewezen krijgt. Op de Job-kaart staat bij wie hij ligt en iedere ingelogde collega kan hem doorzetten ("zet hem even op John, dan kijkt die ernaar"). De Jobs-pagina toont **Mijn** (wat bij jou ligt), **Alle** (alle open Jobs van het team) en **Gesloten**.

`Job inschieten` bewaart de Job en eerste queued Session direct en retourneert vóór Git, Docker en ACP worden gestart. De browser blokkeert dubbel submitten; een duurzame, user-gebonden idempotency-key zorgt daarnaast dat retries of een klikburst server-side dezelfde Job teruggeven. De zware start gebeurt daarna op de achtergrond.

Het kruisje op een Job annuleert een eventuele achtergrondstart, stopt zijn lokale Session-capsules en verwijdert alle bijbehorende lokale workflowstate. De Git-repository en remote Job/Session-branches blijven bewust bestaan.

Een gesloten Job kan door iedere ingelogde collega als vervolg worden geforkt. De nieuwe Job wordt van die user, krijgt een eigen branch en workflow, en gebruikt diens laat gebonden Git-identity, en begint op dezelfde basisbranch als de bron-Job (bijvoorbeeld `develop`), zodat het vervolg dáár landt en niet op de al gesloten branch van de bron. De resultaatbranch van de bron komt als `origin/jobs/<naam-id>/main` read-only mee in de workspace en staat in de prompt genoemd, zodat de agent kan zien wat er toen gemaakt is. De oorspronkelijke goal, laatste revisie van ieder deliverable en alle oorspronkelijke PDF-/afbeeldingsbijlagen gaan als immutable ACP-context mee. Daardoor kan feedback of een nagekomen bug worden opgepakt zonder de oude Job opnieuw te openen. Zolang een vervolg ernaar verwijst, kan de bron-Job niet worden verwijderd.

Iedere workflow-Session krijgt via een intern, kortlevend MCP-kanaal dezelfde kleine set acties:

- `ask(question)` pauzeert voor input;
- `add_deliverable(name, content)` bewaart een benoemde Markdown-bijlage als nieuwe revisie en overschrijft daarmee het hele document; `edit_deliverable(name, old_text, new_text, all)` vervangt alleen een letterlijk stuk tekst in de laatste revisie (old_text moet precies één keer voorkomen, tenzij `all`) en `read_deliverable(name)` geeft de huidige tekst terug. Deze drie bestaan alleen in een fase die deliverables vraagt. Eén Session levert precies één revisie: de eerste keer schrijven maakt haar, iedere latere herschrijving of edit in dezelfde Session werkt diezelfde revisie bij; een Session die niets schrijft laat de vorige revisie staan. Iedere revisie is in de browser te downloaden als `<naam>-r<revisie>.md`;
- `accept(summary)` en `reject(reason)` sluiten de AI-uitkomst af; reject vereist altijd een reden.

Er bestaat bewust geen committool voor de agent. Bij definitieve `ACCEPT` controleert Spin in dezelfde draaiende Session de worktree. In een schrijffase worden dirty files en eventuele agentcommits tot één herkenbare workflowcommit vanaf de oorspronkelijke Session-base gevouwen. Daarna pusht alleen de control plane fast-forward `HEAD` naar `jobs/<naam-id>/main` en verifieert met een nieuwe remote lookup dat die ref exact dezelfde commit aanwijst. Pas daarna kan de volgende fase starten. Een fase zonder schrijfrecht neemt bij ACCEPT niets mee: de agent mag in zijn wegwerp-workspace restoren, bouwen en experimenteren, maar Spin commit niets en bevestigt alleen de onveranderde Session-basis op de remote Job-ref. Remote credentials bereiken de agent nooit.

De Job Goal wordt altijd geïnjecteerd. Een fase ontvangt daarnaast uitsluitend de deliverable-namen die het Template voor die fase aanvinkt, telkens als volledige laatste revisie. Een geselecteerd document is verplichte context: zonder bestaande revisie start de overgang niet.

Vraagt de agent tijdens een beurt toestemming (ACP `session/request_permission`), dan beantwoordt Spin die zelf met toestaan, "altijd" boven "eenmalig", en toont dat als regel in de chat. Zo loopt een Job door, ook bij een agent zonder echte "alles toestaan"-stand. De schakelaar Auto/Vragen in de chatkop zet dat per Session uit; dan komen de vragen bij jou. De standaard voor nieuwe Sessions staat op de ACP-laag zelf, bij Toestemming onder Environments.

Wat een laag geschreven heeft, is van die laag, en elk pad heeft een soort: tool (`/usr`, `/opt`), inlog (`~/.claude`, `~/.codex`, `~/.gemini`, `~/.config/gh`), home, config (`/etc/spin`), workspace, data, of cache (`~/.cache`, `~/.npm`, `/tmp`, `/var/cache`). Bij End & save houdt een laag alleen zijn echte verschil over: Docker rekent een bestand als gewijzigd zodra het voor schrijven is geopend, dus een tool die zichzelf ongewijzigd terugschreef of een login die een hele installatie aanraakte, maakte een laag van honderden megabytes die niets toevoegde. Spin leest de diff van de laag, vergelijkt elk bestand met de laag eronder, laat weg wat byte voor byte gelijk is en wat cache is, en bouwt de laag opnieuw uit de rest als er iets wegviel. De laagkaart toont de inhoudsopgave: aantal bestanden, grootte per soort met de grootste paden, en wat is weggelaten; de knop Inhoud opent de volledige lijst, grootste eerst, met filter op pad en soort, zodat je ziet wat er mis ging als een laag niet bevat wat je bedoelde. Die lijst staat als bestand naast de state, niet erin. Na elke beurt en bij het stoppen noteert Spin ook wat een capsule buiten de workspace veranderde, per soort, op de compositiekaart, met Bekijk voor de lijst.

Een agent ververst zijn OAuth-token terwijl hij werkt, en zo'n refresh-token is eenmalig. Bleef dat in de capsule achter, dan startte de volgende Session met het oude token uit de laag en weigerde de provider ("another process is refreshing it"). Daarom kan een laag bestanden **bijhouden**: in het scherm Inhoud vink je per bestand aan wat Spin tussen Sessions bewaart (een login, een config die de agent bijwerkt), op elke laag. Bij een credential-laag staan de inlogbestanden voorgevinkt tot je zelf kiest. Na elke beurt en bij het stoppen leest Spin die bestanden uit de capsule terug, versleuteld per laag en gebruiker in de database, en zet ze vóór de volgende agentstart weer neer; de keuze gaat mee naar een nieuwe versie van de laag. Alles wat de agent verder in de capsule deed, verdwijnt met de capsule.

De workspace van een agent is beschermd. Git staat klaar met de Session-branch uitgecheckt, de Job-branch en de basisbranch als remote refs, en de branch van een Job die deze voortzet als context; alles wat de Job al deed is dus lokaal te zien en te vergelijken. Credentials komen er nooit in: Spin geeft ze per eigen git-opdracht (checkout, WIP-push, ACCEPT, merge) via stdin mee aan precies die opdracht, als omgevingsvariabelen van dat ene proces, nooit in `.git/config`, nooit in de remote-URL en nooit in de omgeving van de agent. De agent kan lokaal committen, maar niets pushen of ophalen wat autorisatie vraagt; alleen Spin schrijft naar de remote, en alleen naar de Session-branch en bij ACCEPT naar de Job-branch.

Per fase kies je optioneel een **model** en een **reasoning-niveau**. Spin zet die vóór de eerste prompt op de ACP-sessie van de agent, zodat een routinestap een klein model kan draaien en een ontwerpstap een groot, zonder aparte lagen per model. Welke waarden een agent aanbiedt, staat op zijn laag: **Opties ophalen** op een ACP-laag onder Environments start de agent één keer in een wegwerp-capsule, leest wat `session/new` aanbiedt en bewaart dat; iedere gestarte Session ververst ze gratis. De Template-editor biedt die waarden aan. Een waarde die de agent weigert, houdt de fase in de wachtrij met de reden op de kaart.

Het commando van een ACP-laag (`--command=…` bij RECORD) staat op de kaart van de laag en is daar achteraf aan te passen; een nieuwe versie neemt het over. De laag die `acp` ENABLES bepaalt hoe zijn agent start. Na **Opties ophalen** (dat de laag composeert met de lagen erboven, zoals je credential-laag, en de `configOptions` van de agent bewaart) kies je op die kaart de **Modus**, het **Model** en de **Reasoning**; dat zijn metadata van de laag, dus geen herseal, en ze gaan bij een bewerken mee naar de nieuwe versie. Leeg betekent automatisch: de capsule is de sandbox (een wegwerpcontainer zonder host en zonder Git-credentials, waarvan alles verdwijnt wat ACCEPT niet tot een commit vouwt), dus Spin zet de sessie in de full-access-modus zodra de agent die aanbiedt (`session/set_mode`, bij codex-acp `agent-full-access`), en model en reasoning laat hij aan de agent. Een Template-stap kan model en reasoning nog per stap overschrijven. `CODEX_CONFIG` of `config.toml` zijn hiervoor niet nodig.

Dit is generiek per het ACP-protocol, niet per agent. ACP kent drie manieren waarop een agent sessie-instellingen aanbiedt, omdat het protocol gegroeid is: mode-state (`session/set_mode`), model-state (`session/set_model`) en de generieke `configOptions` (`session/set_config_option`) die beide opvolgen. Spin vouwt alle drie tot één lijst instellingen op de categorieën uit de spec (`mode`, `model`, `thought_level`) en zet een keuze via de methode waarmee de agent die instelling aanbood. Codex en OpenCode melden `configOptions`; Claude Code en Gemini CLI melden `modes` en `models`. Een nieuwe agent die de spec volgt werkt dus zonder code voor hem. Welke modus "volledige toegang" is leest Spin uit de markering van de agent (`_meta.kind: full_access`) en anders uit de naam (`agent-full-access`, `bypassPermissions`, `yolo`); kies anders de modus expliciet op de laag.

Het modelveld op de laag accepteert naast de lijst ook een getypt model-id, want de lijst is wat de agent laat kiezen en niet alles wat hij kan. Voor Claude is de actuele adapter `@agentclientprotocol/claude-agent-acp` (commando `claude-agent-acp`; dit is wat Zed gebruikt; de Claude CLI wil een POSIX-shell, dus `apk add --no-cache bash` in die laag, Spin zet `SHELL` voor de agent): die meldt modellen (Default, Opus, Fable, Sonnet, Haiku), effort-niveaus (default tot max) en modi als config options, en markeert zelf welke modus volledige toegang is. De oudere `@zed-industries/claude-code-acp` kent geen effort-niveaus en toont alleen Default, Sonnet en Haiku plus wat je zelf in `~/.claude/settings.json` hebt gezet. Weigert een agent een waarde, dan staat zijn eigen reden in de foutmelding op de kaart.

Een bericht dat je in de chat stuurt terwijl de agent werkt, gaat bij een agent die dat aanbiedt (codex-acp: `_session/steering`) direct de lopende beurt in; anders wacht het als volgende beurt.

### Test-app: de app draaien op de workspace van een stap

Een repository is de app, dus daar hoort het recept om hem te draaien: onder Connections → Git heeft een repository **services**. Iedere service is één container; een service met `run` draait in de image van de Session op haar workspace (dus op exact de code die de agent in die stap bouwde, inclusief niet-gecommit werk), na de `prepare`-commando's in volgorde; een service met `image` is een kant-en-klare dependency zoals `postgres:16`. Alle services van een Session zitten in één Docker-netwerk en bereiken elkaar op servicenaam; iedere gepubliceerde poort krijgt een vrije poort op de runner-host.

```text
web   prepare: npm ci            run: npm run dev       ports: 5173   env: easyflor
api   prepare: dotnet restore    run: dotnet run --project src/Api   ports: 5000   env: easyflor
db    image: postgres:16                                              env: easyflor-db
```

Geheimen blijven op de runner-host. `env` noemt een bestand `var/env/<naam>.env` naast de runner (flag `-env-dir`), dat je bijvoorbeeld met 1Password vult (`op inject -i easyflor.env.tpl -o var/env/easyflor.env`). De runner geeft het met `--env-file` aan de container; de control plane, de backup en de agent-capsule zien de inhoud nooit. `host.docker.internal` wijst naar de runner-host: was een adres `localhost` of `127.0.0.1` op je eigen machine, gebruik dan die naam in je envbestand, want in een container is `localhost` de container zelf. Een database die de app op naam aanspreekt zet je bij de repository onder **Hosts voor de test-app** (`naam ip` per regel); die regels gaan als `--add-host` mee naar iedere service. Dat is bewust een instelling en geen kopie van het hosts-bestand van de runner. De link die de UI toont gebruikt het netwerkadres van de runner (`-advertise-host` om het te kiezen) en werkt binnen dat LAN.

Op iedere actieve stap met een workspace staat het paneel **Test-app**: per service de status, de link en de logs, met Start/Herstart en Stop. Een Template-stap van het soort **Test-app** doet dit automatisch: de workspace komt op vanaf de Job-branch, de services starten en de stap wacht met de links op jouw `ACCEPT` of `REJECT` met reden. Bij een besluit gaat de app samen met de workspace weg.

**Afronden.** Een stap die de Job afrondt zegt in zijn ACCEPT-route hoe: **Job afronden · pull request maken** of **Job afronden · direct mergen in de basisbranch**. Een persoon kiest nooit iets anders dan ACCEPT of REJECT; wat dat betekent bepaalt het Template, ook als de agent zelf accepteert. Het Template geeft onderin de standaard voor een route die alleen "afronden" zegt: als **pull request** op de remote (standaard; jij merget daar), of als **merge** door Spin zelf. Review en acceptatie zijn in Spin al gebeurd, dus met Mergen is er geen pull request meer nodig: de laatste fase brengt een workspace op met de omgeving van de Job en merget daaruit de Job-branch in de basisbranch, altijd als merge-commit met de Job als onderwerp en nooit als fast-forward, zodat de basisbranch één commit per Job toont met de commits van de Job daaronder zichtbaar; daarna pusht Spin en controleert de remote. Een merge die niet schoon lukt, wordt een beslissing voor een persoon, net als een mislukte pull request.

**Tijd in de chat.** Ieder bericht en iedere toolkaart draagt het tijdstip van de server; tijdens een beurt toont de status hoe lang geleden de agent voor het laatst iets deed, en kleurt na drie minuten stilte oranje.

`ASK USER` is per ACCEPT- of REJECT-route een vinkje/gate, geen AI-uitkomst. Zo kan ACCEPT menselijke goedkeuring vragen terwijl REJECT nog automatisch terugloopt. Na bijvoorbeeld `AI ACCEPTED` ziet de gebruiker de vaste routes en kiest die `ACCEPT`, `REJECT` met een eigen reden, of `CHAT`. Bij `REJECT` gaat een nieuwe Session over de geconfigureerde terug-route. Na het ingestelde aantal automatische rejections wordt dezelfde gate getoond, inclusief de laatste reden. `CHAT` opent dezelfde ACP-Session; pas bij het sturen van een bericht wordt die hervat, zonder impliciete goed- of afkeuring. De wachtende beslissing blijft staan: zolang de agent werkt zijn de knoppen dicht, en zodra de beurt eindigt zonder nieuw besluit zijn `ACCEPT` en `REJECT` weer klikbaar.

Deliverables staan als bijlagen bij zowel de fase als de chat en openen als volledig Markdown-document. Bovenin kan tussen alle immutable revisies worden gewisseld. Alleen op de laatste revisie kan een ingelogde gebruiker tekst selecteren en een permanente comment plaatsen; historische revisies en hun bestaande comments zijn read-only. De server controleert bij iedere comment opnieuw of de revisie nog actueel is, zodat een oude browsertab geen retroactieve feedback kan toevoegen. Comments op de laatste revisies gaan als onafhankelijke context naar iedere nieuwe workflow-Session en staan los van ACCEPT/REJECT.

Onder Gesloten staat een zoekveld: afgerond werk is terug te vinden op naam, referentie (ticketnummer) of branch.

Code review volgt hetzelfde immutable model. De algemene `Changes`-knop op een Job opent altijd de volledige boom vanaf de basisbranch; de knop bij een fase opent uitsluitend de laatste diff van die poging. Het openen van deze grote reviewweergave legt precies één dedupliceerde revisie vast. Selecteer tekst of klik een coderegel om een permanente comment met bestand, zijde en regelbereik te plaatsen. Oudere diffrevisies blijven via de revisiebalk terugleesbaar maar kunnen niet achteraf worden aangepast. Draait de capsule van de Job nog, dan komt de diff uit die workspace; is hij gestopt, dan vergelijkt een runner de branches op zijn eigen kale clone van de remote (alles wat de Job deed staat daar al), zonder één laag te herstellen of image te verschepen. Wanneer de bijbehorende workflowpoging wordt gereject, injecteert Spin zowel de rejectreden als alle codecomments in de volgende Session; diens fasediff begint opnieuw klein terwijl de volledige Job-boom bovenin beschikbaar blijft.

De Job toont `BEZIG`, `PENDING · ASK`, `PENDING · USER` of `KLAAR`, plus alle pogingen. Zo blijft de flow generiek terwijl Templateconfiguratie bepaalt waar iedere beslissing heen gaat.

### Runner installeren

Onder Connections → Runners staan downloadknoppen voor `spin-client` (macOS Apple Silicon, Linux arm64 en amd64) van precies de release die de server draait, met de startopdracht erbij. De runner heeft Docker nodig, verbindt uitgaand naar de server-URL en meldt zich met het worker-token. Dat token hoort bij deze Spin en staat in zijn database, versleuteld als elk ander geheim: een admin toont het in datzelfde paneel en kan het vernieuwen, waarna alle runners met het nieuwe token opnieuw starten. Het reist mee in een backup en overleeft dus een herstel; `SPIN_WORKER_TOKEN` in de omgeving zaait alleen nog het eerste token van een database die er geen heeft.

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

De ACP-hook volgt het stabiele ACP v1-transport: newline-delimited JSON-RPC 2.0 over stdio. **ACP probe** op de compositiekaart blijft beschikbaar als korte diagnostische handshake. Voor een Job-Session houdt Spin het subprocess levend en doorloopt het `initialize → session/new → session/prompt`; `session/update`, plannen, tool calls en permission requests worden live naar het chatscherm gestreamd. De Changes-kolom leest de echte Git-workspace en toont per bestand toegevoegde en verwijderde regels.

Onder Access kan een admin een gebruiker een nieuw tijdelijk wachtwoord geven (**Wachtwoord**); alle sessies van die gebruiker eindigen daarbij. Voor een collega die het wachtwoord kwijt is, is dat de route.

## Live status zonder polling

De browser vraagt de staat niet, hij krijgt hem. Eén WebSocket (`/api/state/ws`) stuurt de volledige staat bij verbinden, daarna opnieuw zodra de store een keer is opgeslagen (opeenvolgende opslagen binnen 150 ms worden één bericht) en elke drie seconden zolang er iets alleen in het geheugen beweegt: een launch, een seal, een start, het ophalen van agent-opties, een test-app. Ieder bericht draagt een versienummer dat alleen oploopt. Valt de verbinding weg, dan herstelt de browser die met oplopende wachttijd; na een actie haalt hij de staat één keer direct op.

**Explore** in het menu bladert door een repository uit Connections → Git: kies de repository en een branch (nieuwste eerst, gegroepeerd op prefix), en een runner houdt er een ondiepe clone van bij (één volume per repository) waaruit hij op het moment zelf de branches, de boom en een bestand leest, met de Git-identiteit van wie kijkt. Spin slaat er niets van op. Mappen klappen open, een bestand toont met regelnummers en syntaxkleuring, tot 512 KiB.

Renderen blijft idempotent en volledig, maar de DOM wordt alleen aangeraakt waar de HTML echt verschilt: de Job-lijst per kaart op sleutel, de andere lijsten per regio. Een regio of kaart waarin je bezig bent (een open dropdown, een veld waarin je typt) wordt pas bijgewerkt als de focus die verlaat. Opengeklapte panelen onthouden hun stand.

## API van lagen en capsules

De recorder en de laagkaarten gebruiken deze endpoints; er is geen commandotaal.

```text
POST /api/recordings                      {kind, name, scope, parent_artifact_ids, enables:[{name, command}]}
POST /api/artifacts/{id}/edit             de huidige versie opnieuw opnemen als nieuwe versie
POST /api/recordings/{id}/end             End & save (202 met seal-voortgang, of 201 met het artifact)
POST /api/recordings/{id}/cancel
GET  /api/recordings/{id}/start|seal      voortgang van de start- en seal-jobs
GET  /api/recordings/{id}/terminal        WebSocket-PTY in de opname
POST /api/use                             {selector, with_selectors, profile} of {session_id}
POST /api/compositions/{id}/stop
POST /api/compositions/{id}/acp/probe
GET  /api/compositions/{id}/terminal      WebSocket-PTY in een USE-compositie
```

Houd een laag klein, en Spin helpt daarbij. Elke laag, ook een nieuwe versie van een bestaande, wordt opgeslagen als commit over de laag waarop hij is opgenomen; wat byte voor byte gelijk is aan die laag en wat cache is, valt bij End & save weg (zie de inhoudsopgave). In het archief bewaart een laag die op een andere Spin-laag is opgenomen alleen zijn eigen verschil en de naam van zijn ouder: credential:codex is dan een paar kilobyte, niet de 260 MB van alles eronder. Een runner die de laag nodig heeft, haalt eerst de ouder en bouwt de laag daarop opnieuw op. Docker geeft zo'n opgebouwde laag een ander laag-ID, dus een laag heet naar zijn inhoud: een keten van de identiteit van de ouder en de hash van het eigen verschil, op elke runner gelijk. Een oudere versie waar nog een verschil op steunt, wordt niet opgeruimd. Een wortellaag (op de Alpine-basis) gaat nog als hele image het archief in.

Een laag is een immutable snapshot, dus bewerken is opnieuw opnemen. **Bewerk** op een laag start een opname van de huidige versie met al haar instellingen: scope, profiel, `ENABLES` en entrypoint komen mee, en de opname begint in de bestaande snapshot. Je typt in de shell alleen wat erbij moet, bijvoorbeeld:

```sh
echo "CODEX_CONFIG='{\"sandbox_workspace_write\":{\"network_access\":true}}'" > /etc/spin/enabled/acp.env
```

Opname starten, bewerken en End & save zijn jobs, geen requests: de opname of het artifact bestaat meteen, het werk (basisimage naar de runner, capsule starten, image exporteren en in stukken van 1 MiB archiveren) loopt op de server door en de browser volgt de voortgang via `GET /api/recordings/{id}/start` en `/seal`. Een opname zonder capsule heeft altijd zo'n startjob: valt de server tussendoor weg, dan hervat hij bij het opstarten iedere nog startende opname en geeft de runner de capsule terug die hij daarvoor al had gemaakt. Cancel werkt ook tijdens het starten (de job stopt en de opname vervalt) en heeft geen runner nodig voor een opname die er nooit een kreeg.

End & save maakt het resultaat de nieuwe versie en zet de oude opzij: die verschijnt niet meer in lijsten en selectors, maar haar snapshot blijft bestaan voor lopende Sessions en voor lagen die ervan zijn afgeleid. Die afgeleide lagen volgen de bewerking vanzelf: een compositie die de oude versie in haar closure aantreft (bijvoorbeeld via `credential:codex --from=tool:codex`) bindt de nieuwste versie in dat slot. De Docker-engine neemt die nieuwste versie als basis en past iedere laag die erop gebouwd is toe als haar eigen Docker-diff, dus alleen wat die opname toevoegde, wijzigde of verwijderde (whiteouts); wat de bewerken weghaalde komt zo niet via een oudere afgeleide laag terug. Alleen een laag uit een onafhankelijke keten wordt nog als geheel gekopieerd. Een bewerken van `tool:codex` bereikt zo ook iedere credential- en toolinglaag die erop is gebouwd, zonder die opnieuw op te nemen.

## Runner als HOP-job

`hop-spin-client.example.json` in de root is een kant-en-klare exec-job (kopieer hem naar `hop-spin-client.json`, dat bestand staat in `.gitignore`) voor een HOP-cluster: hij haalt de runner uit de `rolling` release voor macOS arm64, Linux amd64 en Linux arm64 (een redeploy is dus een upgrade), en stelt de runner volledig in via `env`: `SPIN_SERVER`, `SPIN_WORKER_TOKEN`, `SPIN_CLIENT_ID_FILE` (een vaste client-id in de jobmap, zodat een herstart dezelfde runner blijft), `SPIN_ENV_DIR`, `SPIN_MAX_WORKLOADS`; ook `SPIN_CLIENT_NAME` en `SPIN_ADVERTISE_HOST` kunnen daar. Het command is dan alleen `exec ./spin-client`. Een supervisor geeft een kale `PATH` mee; de runner zoekt de Docker CLI daarom zelf op de gebruikelijke plekken (Docker Desktop, Homebrew, `~/.docker/bin`), en met `SPIN_DOCKER` wijs je hem expliciet aan. De job zet daarnaast een `PATH` die Docker Desktop op macOS dekt. Vul `REPLACE_WITH_WORKER_TOKEN` in het cluster in, nooit in git. De node heeft de Docker CLI en socket nodig; app-services lezen hun env-bestanden uit `var/env` in de jobmap.

## Server, client en opslag

- `cmd/spin-server`: HTTP-server, web-GUI, state, Git/OAuth en orchestrator; deze container heeft geen Docker-socket.
- `cmd/spin-client`: reconnectende Docker-runner voor snapshots, Git-workspaces, PTY, ACP en reviewoperaties.
- `internal/capsule`: journalengine en echte Docker commit/clone-engine.
- `internal/store`: persistente Artifactgraph, scope-resolutie, Jobs en Sessions.
- `internal/server`: GUI, REST, commandparser en de langlevende ACP-session-supervisor.
- `var/spin.db`: centrale SQLite-database met control-plane-state, Job-bijlagen en de opaque exports van iedere afgeronde Docker-snapshot.
- `var/spin-master.key`: lokale AES-masterkey; apart van de state back-uppen en nooit publiceren.
- `var/spin-worker.token`: apart bearer-token voor het headless runner-WebSocket.
- `var/spin-client.id`: stabiele runneridentiteit; blijft gelijk over containerrestarts.

Nieuwe workloads worden round-robin over online, niet-drainende runners verdeeld. Een admin kan een runner handmatig drainen: bestaand werk en zijn vaste affinity blijven bereikbaar, maar nieuw werk slaat hem over totdat hij wordt hervat. Zodra een Recording of Session een runner heeft, bewaart zijn runtime die `client_id`: een kort netwerkverlies verandert nooit de uitvoerder. Zowel server als client sturen WebSocket Ping-frames, eisen tijdige Pong-frames en vervangen een verbroken socket met exponential backoff. Pending RPC's gebruiken stabiele request-ID's en worden na reconnect idempotent hervat; ACP- en PTY-streams blijven aan dezelfde logische client gekoppeld.

Een nette SIGTERM stuurt best-effort `goodbye` met de lokale idle-status. Een harde Docker-kill of ontbrekend internet is nadrukkelijk geen bewijs dat de workload dood is en veroorzaakt dus geen automatische failover. De Job toont bij de actieve fase welke client ontbreekt en hoe lang die offline is. `Retry` behoudt de logische fasepoging, verbreekt bewust de oude runtime-affinity en materialiseert een nieuwe Capsule via round-robin. Een later terugkerende oude runner kan die opnieuw gekoppelde Session niet meer overnemen.

Een snapshot wordt herkend aan zijn lagen (de diff-ID's van de image), niet aan zijn image-ID: de klassieke image store en de containerd image store van Docker geven dezelfde image een ander ID, en runners met verschillende Docker-versies moeten elkaars lagen kunnen laden. Docker-images zijn runner-lokale caches, niet langer de bron van waarheid. Bij End & save exporteert de runner eerst een opaque `docker image save`, gzip-gecomprimeerd aan de bron (2 tot 3 keer kleiner; `docker load` leest dat zelf), en uploadt die in losse, geackte 1 MiB-chunks naar `spin.db`, over dezelfde `/api/uploads`-API waarmee een browser een backup terugzet; pas na die duurzame archivering wordt het Artifact afgerond. Een verbroken verbinding hervat op de bevestigde offset, en een herhaalde `END RECORD` vindt een al gecommitte image terug. Wanneer een nieuwe Session op een andere runner landt, gebruikt Spin een online replica of laat de runner de centrale export zelf ophalen: in stukken van 1 MiB over HTTP (`GET /api/snapshots/{digest}?offset=`), vier stukken tegelijk onderweg zoals bij een upload, ieder stuk apart bevestigd en herhaald, gespoold en pas daarna in Docker geladen. De runnerverbinding draagt dan alleen de voortgang; een wegvallende lijn pauzeert het ophalen in plaats van de stap te laten mislukken. Het ophalen hoort bij de runner, niet bij de aanvraag: geeft de aanvrager het op (een vergelijking van Job-wijzigingen onder de proxylimiet), dan haalt de runner de image toch af en vindt de volgende poging haar; twee aanvragen voor dezelfde image op dezelfde runner delen één download. Een nieuwe workspace start bij voorkeur op een runner die al alle images heeft, en pas anders round-robin. Een oudere runner krijgt de export nog over de verbinding geduwd. De nieuwe cachekopie wordt onthouden. Layerinhoud wordt niet geïnterpreteerd en een verdwenen laptop vernietigt dus geen Artifact.

De volume onder `spin.db` is eindig en HopOS kan niet zeggen hoeveel er nog vrij is. Spin verspilt daarom niets: bewerken maakt de oude versie van een laag vervangen, en zodra geen draaiende compositie en geen open opname die versie nog gebruikt, verwijdert Spin haar gearchiveerde snapshot (bij het opstarten en na iedere opgeslagen laag). De nieuwere versie draagt de inhoud; een runner die de oude image nog heeft, houdt die als cache. De vrijgekomen pagina's hergebruikt SQLite voor nieuwe uploads; het bestand krimpt niet. Onder Connections → Runners staat wat de database inneemt en hoeveel oude versies nog op opruimen wachten; `/healthz` meldt hetzelfde. Een upload die de opslag weigert (een volle volume) antwoordt met 507 en zegt dat erbij. Lagen van vóór de gzip-export staan ongecomprimeerd in het archief tot een nieuwe versie ze vervangt. **Backup** streamt de live database rechtstreeks naar de browser, zonder kopie op de volume: een zip met `spin.db` en `master-key.txt` (de portable sleutel zit bewust niet in de live database). Zolang de download loopt houdt Spin zijn enige databaseverbinding vast en pauzeert dus iedere schrijfactie; een runner die een laag uploadt krijgt 503 met Retry-After en wacht tot een half uur. Restore accepteert die zip. Kopieën die een eerder proces achterliet ruimt de server bij het opstarten op.

Die opslaggrens is bewust hard: uitsluitend afgeronde, opgeslagen lagen worden centrale artifacts. Tijdelijke composities, containers, Session-worktrees en hun Docker-delta's blijven wegwerpcache op de runner. Retry begint opnieuw bij de opgeslagen artifacts en de actuele remote Job-branch; er hoeft geen half afgemaakte runtime te worden verhuisd.

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

De Session-container heeft gewoon netwerk (`-capsule-network bridge`), maar de sandbox van Codex zelf staat in de standaardmodus `agent` (workspace-write) zonder netwerktoegang. Een `dotnet restore` of `npm install` strandt dan op de packagebronnen, ook al kan de container ze bereiken. codex-acp leest `CODEX_CONFIG`, een JSON-object dat in de Codex-sessieconfig wordt gemerged; de `tool:codex`-opname hierboven zet daarmee het netwerk aan. Wil je dat los van de toollaag kunnen schakelen, dan kan diezelfde regel ook in een aparte `config:`-laag die je per repository of als `WITH`-laag op een fase meegeeft. Spin interpreteert de variabele niet; dezelfde haak draagt ook `INITIAL_AGENT_MODE` (`read-only`, `agent`, `agent-full-access`) of een andere ACP-wrapper.

Bij `session/new` geeft Spin zowel `/workspace` als de capsule-HOME `/root` door. ACP-agents die `additionalDirectories` ondersteunen nemen HOME daardoor op als writable root van hun workspace-sandbox. Gewone tooling kan dus zonder productspecifieke uitzonderingen naar bijvoorbeeld `/root/.dotnet`, `/root/.npm` of `/root/.cache` schrijven. Dit is uitsluitend `/root` ín de geïsoleerde, gematerialiseerde Session-container; de host en de immutable bronsnapshot worden niet schrijfbaar. Een laag kan `/etc/spin/enabled/acp.env` nog steeds gebruiken voor aanvullende runtimeconfiguratie zoals netwerkbeleid. Dat bestand wordt als shell ingelezen (`set -a; . acp.env`), dus een JSON-waarde moet in enkele aanhalingstekens staan: `CODEX_CONFIG='{"…"}'`; zonder die quotes eet de shell de dubbele aanhalingstekens op en krijgt de agent ongeldige JSON.

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

Connections → Runners toont online/offline/draining, engine, capaciteit, last-seen en hoeveel Sessions duurzaam aan iedere client hangen. Admins kunnen daar nieuwe plaatsing per runner drainen en hervatten, en een offline runner zonder werk verwijderen. Een runner is dezelfde runner over herstarts heen: zijn identiteit volgt de machine en de naam (of een identiteitsbestand van een oudere installatie), ook wanneer hij als HOP-job telkens uit een lege map start. Offline runners waar niets meer aan hangt (geen Session, geen draaiende capsule of opname) ruimt de server na een dag zelf op; een runner die terugkomt meldt zich onder dezelfde identiteit opnieuw aan.

### Releases

`release.sh` bouwt één matrix en publiceert die zowel onder een immutable versie als onder de overschrijfbare `rolling`-release:

```sh
./release.sh v1.0.0
```

De Linux-server en -client worden statisch gebouwd voor amd64 en arm64. De client draait ook in Docker nog steeds als native binary van de hostarchitectuur; Docker levert de verpakking, Docker CLI en socket, geen CPU-emulatie. Daarnaast bouwt het script echte HopOS/Tamago server-ELF's voor arm64 en riscv64 tegen `${HOPOS_DIR:-$HOME/Git/hop-os}`. Met `PUBLISH=0` voer je alleen de volledige compile-gate uit.

Een client krijgt alleen de publieke server-URL. `https://spin.example.test` wordt automatisch `wss://spin.example.test/api/runner/ws`; bij disconnect gebruikt hij ping/pong en exponential reconnect. Server en client delen uitsluitend het worker-token van dat domein.

### Domeinen en databases

Eén server draait meerdere Spins, één per domein. Het `Host`-header kiest de Spin: `bollenloods.getspin.app` krijgt `bollenloods.getspin.app.db` in de datamap (`SPIN_DATA_DIR`, standaard `/data` op HopOS en `./var` elders), met een eigen store, eigen runners, eigen worker-token en eigen replica. De server neemt elk domein aan dat binnenkomt; wat ervoor routeert bepaalt wat er binnenkomt, er hoeft niets vooraf te worden vastgelegd. Een IP-adres als host is nooit een Spin en beantwoordt alleen `/healthz`. Bij het opstarten opent de server elke Spin die hij al heeft (de databases in de datamap, en de domeinen met een replica in de bucket, die zo nodig eerst worden hersteld), zodat hun replica's vanaf de eerste minuut lopen. Een bezoek aan een Spin die nog opent, wacht twee seconden; duurt het langer (een herstel uit de bucket), dan antwoordt de server met de stap waar het staat en de pagina toont die met een draaiend icoon en vraagt het elke twee seconden opnieuw. `SPIN_DOMAINS` (kommagescheiden) kan de lijst optioneel beperken. `SPIN_DATABASE` zet de server terug in de oude vorm: één database voor elke host. `SPIN_PUBLIC_URL` is per domein `https://<domein>` tenzij anders gezet; `SPIN_MASTER_KEY` geldt voor alle databases op de server.

### Replica in S3

Een Spin zonder replica is geen Spin: de server start niet zonder `SPIN_S3_ENDPOINT`, `SPIN_S3_BUCKET`, `SPIN_S3_ACCESS_KEY` en `SPIN_S3_SECRET_KEY` (optioneel `SPIN_S3_REGION`, standaard `auto`, en `SPIN_S3_PREFIX`, standaard `spin`). Alleen een ontwikkelserver zet `SPIN_REPLICATION=off`. De onafhankelijke Go-bibliotheek in [`replica/`](replica/README.md) volgt SQLite-pagina's via een VFS. Iedere vijftien seconden legt één leestransactie alle vuile pagina's vast in een lokaal spoolbestand; daarna wordt de database vrijgegeven en begint de upload. Pas een manifest na alle segmenten publiceert het volledige herstelpunt. Een generatie begint met een blijvende snapshot van alle pagina's; `current` wijst naar een complete generatie. Na een week of voldoende wijzigingen begint een nieuwe generatie; generaties blijven vier weken staan.

Herstelpunten dunnen uit met de leeftijd: kwartieren voor twee uur, uren voor een dag, dagen voor een week en wekelijkse generaties voor een maand (`SPIN_REPLICA_SCHEDULE=15m:2h,1h:24h,24h:168h`, `SPIN_REPLICA_GENERATION=168h`, `SPIN_REPLICA_RETENTION=672h`). Een venster bevat de laatste staat van iedere gewijzigde pagina en wordt pas zichtbaar wanneer alle delen geüpload zijn. Alleen een compleet venster mag fijnere bestanden vervangen. De oorspronkelijke snapshot blijft apart bewaard. Herstel volgt vanaf die snapshot aaneengesloten reeksen complete wijzigingen; ontbrekende delen en checksumfouten breken herstel af. Downloads landen eerst in een tijdelijk bestand. Een duurzaam herstelmerkteken zorgt dat een onderbroken publicatie bij de volgende start opnieuw wordt uitgevoerd, ook als het databasebestand al bestaat.

Onder Backup & restore toont een admin de herstelpunten en zet er een terug via dezelfde gevalideerde restore als een backup-zip. De staat van vóór het herstel blijft zelf een herstelpunt. De opslagregel onder Connections → Runners en `/healthz` melden bucket, generatie, laatste sync, wachtende pagina's en fouten. Spin levert de databaseadapter, omgevingsvariabelen en levenscyclus; de replicatiebibliotheek heeft eigen tests met SQLite, vervangbare opslag, een nepklok en geïnjecteerde fouten. Zie de [pakketdocumentatie](replica/README.md) voor gebruik buiten Spin, het opslagprotocol en tests.

Beide servervarianten, tenantdetectie, status en Backup & restore gebruiken rechtstreeks dezelfde replicatiebibliotheek. Het bestaande opslagpad blijft gelden: er wordt geen extra versieprefix toegevoegd. Bij een nieuwe start kan de normale Backup & restore-interface een backup importeren; de databasewrites lopen door de tracking-VFS en worden bij de volgende sync gerepliceerd. Een lokale replica-marker wordt alleen hergebruikt als hij bij dezelfde opslagbestemming hoort.

`hop-spin-server.example.json` in de root is de HOP-job voor de server (kopieer naar `hop-spin-server.json`, dat bestand staat in `.gitignore`): de Tamago-ELF's uit de `rolling`-release voor arm64 en riscv64, `/data` als volume, en alle instellingen als `env`. De HopOS-server verwacht een gepubliceerde `ER_PORT_HTTP`, een gemounte `/data` voor de databases, de S3-instellingen en een vaste base64 `SPIN_MASTER_KEY`. Let op: het lokale volume van HopOS is scratch en is leeg na een herstart; de replica in S3 is de duurzame kopie, en bij het opstarten haalt Spin elke database daaruit terug. SQLite v0.35.4 is libc-vrij via wasm2go; een eigen HopOS-VFS vertaalt random-access pagina-I/O naar de volume-ABI. De echte releasegate bouwt dit pad voor arm64 én riscv64. Control plane, runner-WebSocket, Job-bijlagen, centrale Docker-snapshots, databasebackup en replica zijn daardoor op beide targets beschikbaar.

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
