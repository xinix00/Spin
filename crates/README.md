# Rust-port van Spin

De Rust-port draait met een **HopOS-server en macOS-runners**. De native
ARM64-server is op HopOS getest met harde herstarts, meerdere domeinen en
herstel uit S3 op een lege schijf. De macOS ARM64-runner is met echte
Docker-containers tegen de native HopOS-server getest: opnemen, uitvoeren,
sealen, uploaden, de lokale image verwijderen, centraal terughalen en stoppen.
De bestaande Bollenloods-database is op de HopOS-node hersteld. Controleer bij
een overname behalve HTTP ook de publieke WebSockets en de Replica-status;
de Rust-app `cloudflared-lean` ondersteunt nog geen WebSockets. Gebruik
daarvoor de volledige Go-cloudflared op een HopOS-kernel met Go-ABI-ondersteuning.

## Indeling

`domain` bevat het bestaande JSON-contract; `core` de platformonafhankelijke
logica, ACP, runnerprotocollen en Docker-opdrachten; `store` de duurzame
transacties; `security` de encryptie en authenticatie; `persistence` de
SQLite-opslag; `server` de HTTP-routes en app-eigenaar. Deze crates gebruiken
`no_std`. `runtime` bevat netwerkafhandeling en hostrouting. `hopos` levert
de native boot en platformadapters. `host` levert de macOS-runner en een
lokale server die alleen voor integratietests bedoeld is.

Replica wordt hergebruikt uit `third-party/replica`; `SOURCE.json` vermeldt
de herkomst, oorspronkelijke hashes en lokale patches;
de manifestgrenzen zijn verruimd voor bestaande productiedatabases (16 MiB JSON,
32.768 delen). De Hop-types-parser heeft daarvoor een expliciet bytebudget;
het standaardbudget voor HTTP-JSON blijft 1 MiB. De C-bronnen zijn ongewijzigd.
Herstel schrijft aaneengesloten pagina's in blokken van maximaal 64 KiB.
Onderbroken S3-GETs krijgen maximaal vier pogingen; muterende verzoeken
behouden Replica's bestaande afhandeling. Eén begrensde keep-alive-verbinding
voorkomt dat S3-verzoeken de NAT-tabel vullen. Retentie vraagt generatiemappen
op met een delimiter en verwijdert maximaal 128 verlopen objecten per ronde,
met herstelmetadata vóór data en zonder de huidige generatie te verwijderen. Lean TLS gebruikt de reeds gepinde,
allocatievrije RustCrypto AES-GCM-implementatie met sleutelwissing; suite,
recordgrenzen en certificaatcontrole blijven gelijk. Lean HTTP neemt een
requestbody ook met `Transfer-Encoding: chunked` aan (cloudflared stuurt een
lege browser-POST zo door; dat was een 501), zoals leanhttp v3.1.6. De
wijzigingen staan in `third-party/rust/LOCAL_PATCHES.json`.
Dependencies zijn vastgelegd in `.cargo/config.toml` en `Cargo.lock`:
HopOS SDK alpha.18, Lean v3.1.2 en de bestaande lokale Replica-snapshot.

## Bouwen en controleren

```sh
CARGO_INCREMENTAL=0 cargo test --offline --workspace
CARGO_INCREMENTAL=0 cargo clippy --offline --workspace --all-targets -- -D warnings
CARGO_INCREMENTAL=0 SQLITE_CC=/opt/homebrew/opt/llvm/bin/clang SQLITE_AR=/opt/homebrew/opt/llvm/bin/llvm-ar cargo clippy --offline -p spin-hopos --target aarch64-unknown-none-softfloat --target riscv64gc-unknown-none-elf -- -D warnings
PUBLISH=0 ./release.sh
```

De laatste opdracht bouwt de runner voor deze Mac plus beide native
HopOS-targets. ARM64 is gebootst en getest; voor RISC-V is
de controle beperkt tot compileren en linken.

De native proef gebruikt uitsluitend tijdelijke VM-schijven, een lokale
S3-server met SigV4-controle en een lokale SNTP-server die de echte UTC van
de Mac doorgeeft. `HOPOS_SDK` wijst naar SDK alpha.18; `HOP_DIR` naar een
compatibele Hop-checkout. De gebruikte Hop-commit is `196b1d3`.

```sh
python3 tools/port-native/qemu.py --s3 --tenants
python3 tools/port-native/qemu.py --s3
# Vereist een reagerende Docker Desktop op deze Mac:
python3 tools/port-native/qemu.py --runner
```

De Docker-proef beperkt inventarisatie en opruimen tot eigen fixturelabels.
Hij start de macOS-releasebinary, maakt een opname, voert een opdracht uit,
sealt en uploadt de laag, verwijdert de lokale fixture-image, haalt de laag
terug van HopOS, materialiseert haar en controleert de inhoud voordat de
capsule stopt. Het testscript herstart Docker Desktop niet.

`release.sh` in de root publiceert de drie artefacten (beide HopOS-ELF's en de
macOS-runner) als GitHub-release `v<versie>` plus `rolling`, met `SHA256SUMS`.
De macOS-binary krijgt een lokale ad-hoc-handtekening; hij is niet genotariseerd.

## Serverconfiguratie

Gebruik één server per datavolume met Hop `count: 1` en
`update_policy: "recreate"`. De bestaande Replica-lease beschermt het hele
volume. Zijn heartbeat heeft een eigen systeemverbinding. Alle domeinen
delen de tweede HopOS-systeemverbinding en één exclusief geleende SQLite-arena.
Netwerktaken blijven buiten de parkeerbare opslagstacks doorlopen.
De Replica-beurt loopt in drie stappen: de eigenaar doet de capture, een eigen
uploadtaak per tenant zet de delen naar S3 terwijl de eigenaar verzoeken blijft
bedienen, en de eigenaar rondt af met manifest en marker. Vóór die splitsing
parkeerde de eigenaar 13 tot 17 s per beurt van 15 s op de Bunny-PUT's
(gemeten 02-10-2026 op bollenloods).
De eigenaar en de boot-lus tikken niet meer elke 10 ms: een sockettaak of de
uploader belt de eigenaar (`Mailbox::nudge`) zodra er iets in een slot ligt, en
de eigenaar slaapt anders tot een vloer van één seconde (10 ms zolang er werk is
dat alleen door pollen vordert: wachtwoorden, uitgaande HTTP, een staat-push).
De boot-lus wacht op werk of een deadline van de domeinontdekking, met een
uitstelbare vangrail van 10 ms die een slapende core niet wekt (HopOS-handboek §4).

| Variabele | Betekenis |
| --- | --- |
| `SPIN_DATA_DIR` | Persistente volumemap; standaard `/data/spin`. |
| `SPIN_PORT` | Native HTTP-poort; standaard `8080`. |
| `DNS` | IPv4-adres van de lokale DNS-resolver als Hop geen `DNS`/`HOP_DNS` meegeeft; nodig voor S3 en overige hostnamen. |
| `SPIN_DOMAINS` | Optionele lijst toegestane DNS-hosts, gescheiden door komma's of witruimte. |
| `SPIN_DATABASE` | Expliciete modus met één database voor alle hosts; een bestandsnaam binnen `SPIN_DATA_DIR`. |
| `SPIN_MASTER_KEY` | Bestaande base64-master key; vereist vóór S3-herstel. Bewaar hem buiten het volume. |
| `SPIN_S3_ENDPOINT`, `SPIN_S3_BUCKET` | Replica-bestemming. |
| `SPIN_S3_ACCESS_KEY`, `SPIN_S3_SECRET_KEY` | S3-credentials. |
| `SPIN_S3_REGION`, `SPIN_S3_PREFIX` | Standaard `us-east-1` en `spin`. |
| `SPIN_REPLICATION` | Alleen met `off` is lokale ontwikkeling zonder S3 toegestaan. |
| `SPIN_PUBLIC_URL` | Optionele vaste publieke URL; anders `https://<domein>` in domeinmodus. |
| `SPIN_INTERNAL_URL` | Optionele URL die vanuit capsules bereikbaar is; standaard de publieke domein-URL. |
| `SPIN_WORKER_TOKEN` | Optionele eerste token-seed; een al opgeslagen of geroteerd token wint. |

De huidige native grens is **acht domeinen en 64 gelijktijdige verbindingen**.
SQLite gebruikt één gedeelde werkruimte van 256 MiB met een paginacache van
16 MiB. Daardoor blijft ruimte over voor het herschrijven van grote bestaande
state-rijen, ook in oudere Go-tabellen met `WITHOUT ROWID`.
Ieder domein heeft zijn eigen Store, database, gebruikers, sessies, runners en
Replica-namespace. Zonder `SPIN_WORKER_TOKEN` krijgt elk domein een eigen
token. Een expliciete seed wordt, overeenkomstig Go, bij ieder nieuw domein
gebruikt. Hostnamen worden genormaliseerd; IP-hosts beantwoorden uitsluitend
`/healthz`. `SPIN_DATABASE` schakelt deze domeinscheiding expliciet uit.

Nieuwe databasebestanden hebben een SHA-256 van de domeinnaam als bestandsnaam.
Een bestaande Go-database `<domein>.db` wordt op haar huidige pad geopend, met
dezelfde Replica-sidecars en S3-namespace. Als beide bestandsnamen bestaan,
wordt openen geweigerd totdat de juiste bron is vastgesteld. De native eigenaar
staat maximaal 16.777.216 SQLite-pagina’s toe (64 GiB bij pagina’s van 4 KiB).
Lokale `.domain`-bestanden en de kleine S3-index `<prefix>/.spin-domains/`
heropenen bekende domeinen zonder eerst een bezoek af te wachten. De allowlist
blijft daarbij gelden. Nieuwe domeinen krijgen tijdens openen een pagina of
HTTP 503 met `Retry-After`; `/api/opening` geeft de voortgang. Gezonde domeinen
en IP-liveness blijven bereikbaar tijdens S3-wachttijd van een ander domein.

Een HTTPS-publieke URL gebruikt `__Host-spin_session` met `Secure`, ook achter
de Hop-proxy. Houd de publieke en capsule-URL op het juiste tenantdomein.
Go-data kan via de portable backup met de bijbehorende sleutel worden
geïmporteerd. Een overname gebruikt de bestaande master key en Replica-lineage;
onbewezen lokale data mag een bestaande remote replica niet vervangen.

## macOS-runner

De runner gebruikt `SPIN_SERVER` en `SPIN_WORKER_TOKEN` of
`SPIN_WORKER_TOKEN_FILE`. Geef iedere runner een eigen `SPIN_CLIENT_NAME`;
`spin-client --help` toont de overige opties. Docker Desktop moet bereikbaar
zijn. HopOS voert zelf geen Docker-opdrachten uit.

Runnerprocessen blijven bij hun eigenaar tijdens een WebSocket-reconnect.
Herhaalde opdrachten worden niet opnieuw uitgevoerd. Een geweigerde identiteit
of token stopt de runner met een duidelijke fout. Bij afsluiten wordt een
begrensde goodbye verstuurd en worden eigen processen opgeruimd.

## Een goede buur op HopOS

De lessen uit `hop-os/docs/apps.md` zijn op de port toegepast (04-10-2026):

- Sockets houden geen executor-timer vast: `crates/hopos/src/conn.rs` bewaart
  lees- en schrijftermijnen zelf en de boot-lus slaapt met één `until` op de
  vroegste. Met 64 verbindingen en 32 timerplekken spinde de core anders stil.
- De eigenaar slaapt tot zijn deurbel of zijn eerstvolgende echte deadline
  (onderhoud 1 s, staat-push 150 ms, watch-controle 3 s), yieldt bij gesneden
  rekenwerk (wachtwoorden, een lopend herstel) en pollt uitgaande HTTP met een
  echte waker.
- Eén geserialiseerde snapshot per staatversie voor alle browsers; de
  opslagtelling eens per 30 s; de launch-sweep filtert oude pogingen met een
  opzoeking vóór hij iets kloont.
- Hartslagen: een tijdelijke weigering (1011/1012, identiteit bezet) stopt de
  runner niet meer, herverbinden heeft een backoff van 1 tot 30 s, en de
  starttimer van een agent loopt niet terwijl zijn runner los is.
- De meetlat op de console: `SPIN_OWNER_LOAD`/`SPIN_OWNER_SLOW`/`SPIN_OWNER_QUEUE`
  per eigenaar, `SPIN_EXEC` (met `timer_overflows`) en `SPIN_BOOT_SLOW` per
  node, `SPIN_CAPSULE_WAITING`/`SPIN_RUNNER_REFUSED`/`SPIN_AGENT_FAILED`/
  `SPIN_SESSION_WAITING_LOGIN` zodra iets wacht, naast de `HOPOS_SLOT_LOAD`
  van de kern. Nog niet gedaan: de volledige kopie van de staat bij elke
  mutatie (`Store::edit`, `encrypt_state`, één rij per document).

## Gecontroleerde functies

- Native HTTP, authenticatie, CSRF, duurzame browsersessies, harde herstart,
  lege-schijfherstel en scheiding van gebruikers, tokens en blobs tussen domeinen.
- Bestaande Replica, native lease-heartbeat, SigV4, chunked S3-antwoorden,
  herstelpunten en zeventig healthrequests tijdens opgehouden S3-I/O.
- Uploads met willekeurige chunkvolgorde, herhaalde chunks, SHA-256-controle,
  centrale snapshots, attachments en deliverables.
- ZIP64-backupdownload en atomair herstel van state plus blobs; een beschadigd
  archief laat de actieve database intact. Ook de legacy `/api/restore` werkt
  boven 1 MiB, met JSON- of NDJSON-voortgang. Onafhankelijke ZIP- en SQLite-tools
  controleren de archieven. Tests omvatten deflate, ZIP64 en data descriptors.
- Browser- en runner-WebSockets, ACP, gedeelde agents, prompts, workflowcontext,
  terminalstreams, Git, merges, remotevergelijking, pull requests en OAuth.
- Retry en beheer van Jobs, capsule-starts en stops, handmatige sessies,
  laagverwijdering, login capture/wisselen/parkeren/verwijderen en agentopties.
- Een parentwissel bij een opname hervat na herstart en wacht eerst op de
  verwijderbevestiging van de oude container. Een onzekere login-write behoudt
  de reservering totdat de capsule aantoonbaar gestopt is.
- Bestandswatchers publiceren hun beginsnapshot, melden procesuitval en worden
  na een bevestigde selectie opnieuw ingericht. Nieuwe bijlagen worden ook in
  bestaande capsules geplaatst; PDF-downloads ondersteunen enkele bytebereiken.
- Opslagstatus, verouderde Replica-synchronisatie, veilige snapshotpruning en
  duurzame herpogingen voor blobopruiming.

De frontend staat in `crates/runtime/ui`; `VERSION` daar is de assetversie en
valt onder de root-`AGENTS.md`. HTML en API-antwoorden blijven niet-cachebaar.

## Geteste platforms

HopOS ARM64 en macOS ARM64 zijn samen met echte Docker-containers getest.
De RISC-V-server is gebouwd en gelinkt, maar nog niet op RISC-V geboot.
De macOS-binary is lokaal ondertekend; Apple-notarisatie is niet uitgevoerd.
Controleer bij een productieovername afzonderlijk dat het bestaande domein
`/healthz` met 200 beantwoordt en `/api/auth/status` `configured: true` meldt.
Een lopend proces of een nieuwe setup-pagina bewijst geen geslaagde overname.
Controleer ook de publieke proxyroute en zorg dat Hop de artifact-URL na een
nodeherstart kan bereiken.
