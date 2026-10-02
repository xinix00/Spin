//! De expliciete Replica-levensloop van één app-eigenaar; geen verborgen threads.
use crate::{
    Error, Result,
    local::Name,
    maintenance::{self, Budget, Report, Schedule},
    marker::{LocalMarker, Marker},
    object::Store,
    prepare::{self, Prepared, Reason},
    replication::{self, Batch, Staged, Uploaded},
    string,
    time::Time,
    tracking::{Tracked, Tracking},
};
use alloc::string::String;
use replica_sqlite::Storage;
/// Configuratie vóór Prepare. De app bezit transport, klok, entropie en lease-heartbeat.
pub struct Config {
    /// `prefix/domain` in objectopslag.
    pub namespace: String,
    /// Bestemmingshash uit `marker::destination`.
    pub destination: String,
    /// Canoniek databasepad in de VFS.
    pub path: Name,
    /// Hard maximum voor tracking/coverage-pagina's.
    pub pages: u32,
    /// Paginabytes per segment: 64 KiB..16 MiB, standaard 4 MiB.
    pub segment_bytes: usize,
    /// Synchronisatie-interval in hele seconden, standaard 15.
    pub interval: u32,
    /// Generatieleeftijd in seconden, standaard één week.
    pub generation: u64,
    /// Generatiebewaartermijn in seconden, standaard vier weken.
    pub retention: u64,
    /// Bewaarschema voor incrementen binnen één generatie.
    pub schedule: Schedule,
    /// Expliciete toestemming voor lokale adoptie zonder bestaande marker.
    pub adopt_local: bool,
}
impl Config {
    /// Defaults zoals Go, met expliciet databasebudget en bestemming.
    pub fn new(namespace: &str, destination: &str, path: Name, pages: u32) -> Result<Self> {
        let out = Self {
            namespace: string(namespace)?,
            destination: string(destination)?,
            path,
            pages,
            segment_bytes: 4 << 20,
            interval: 15,
            generation: 7 * 86400,
            retention: 28 * 86400,
            schedule: Schedule::defaults()?,
            adopt_local: false,
        };
        out.validate()?;
        Ok(out)
    }
    fn validate(&self) -> Result {
        if self.namespace.is_empty()
            || self.namespace.ends_with('/')
            || self.namespace.contains("..")
            || self.namespace.len() > 512
            || self.pages == 0
            || self.interval == 0
            || !(65536..=16 << 20).contains(&self.segment_bytes)
            || self.generation == 0
            || self.retention == 0
            || self.generation > i64::MAX as u64
            || self.retention > i64::MAX as u64
        {
            return Err(Error::State);
        }
        Marker::new(&self.destination, "", Time::ZERO)?;
        self.path.suffix(".replica-restore-data")?;
        Ok(())
    }
}
/// Eén afgeronde beurt; onderhoud kan ook bij een ongewijzigde database plaatsvinden.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Synced {
    /// Er is een snapshot of increment bevestigd.
    pub published: bool,
    /// Aantal gecompacteerde en geprunede manifesten.
    pub maintenance: Report,
    /// Verwijderde objecten uit verlopen generaties.
    pub expired_objects: usize,
}
/// Status zonder strings uit een externe foutmelding of geleende buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// Reden van de laatste Prepare of snapshotvernieuwing.
    pub reason: Reason,
    /// Tijd van de laatste synchronisatiepoging, ook bij een fout.
    pub attempted: Option<Time>,
    /// Tijd van de laatste volledig geslaagde beurt.
    pub synced: Option<Time>,
    /// Laatste fout; transportfouten maken de generatie niet beschadigd.
    pub error: Option<Error>,
}
/// Eén eigenaar van marker, dirty logs en cadans. Alle SQL-writes lopen via `vfs`;
/// SQL is gesloten vóór `sync`/`tick`. Voor overlappende SQL en upload kan een app
/// de losse capture/publicatie-API gebruiken met expliciete eigendomsoverdracht.
///
/// De app garandeert exclusieve starts en bedient een eventuele lease-heartbeat
/// ook terwijl opslag/netwerk parkeert. Bij leaseverlies stopt hij deze eigenaar.
/// Deze bibliotheek verstopt geen achtergrondtaak, runtime of proceslock.
pub struct Replica {
    config: Config,
    prepared: Prepared,
    status: Status,
    maintenance_at: Option<Time>,
    closed: bool,
    pending: bool,
}
/// Een capture waarvan de delen buiten de eigenaar worden geüpload, tussen
/// [`Replica::begin`] en [`Replica::finish`].
pub struct Pending {
    batch: Batch,
    staged: Staged,
}
impl Pending {
    /// Uploadt de delen; raakt alleen de spool en de store, nooit SQLite,
    /// marker of tracking, dus dit mag op een eigen stack naast de eigenaar.
    pub fn upload<B: Storage, S: Store>(&self, b: &mut B, store: &mut S) -> Result<Uploaded> {
        self.batch.upload(b, store, &self.staged)
    }
}
impl Replica {
    /// Prepare vóór SQLite openen, inclusief geverifieerde restore waar nodig.
    pub fn prepare<B: Storage, S: Store>(
        b: &mut B,
        store: &mut S,
        config: Config,
        now: Time,
        verify: impl FnMut(&mut B, &Name) -> Result,
    ) -> Result<Self> {
        config.validate()?;
        let prepared = prepare::run(
            b,
            store,
            prepare::Options {
                namespace: &config.namespace,
                destination: &config.destination,
                path: config.path,
                page_limit: config.pages,
                now,
                adopt_local: config.adopt_local,
            },
            verify,
        )?;
        let status = Status {
            reason: prepared.reason,
            attempted: None,
            synced: None,
            error: None,
        };
        Ok(Self {
            config,
            prepared,
            status,
            maintenance_at: None,
            closed: false,
            pending: false,
        })
    }
    /// De getrackte VFS. De lening sluit gelijktijdig synchroniseren of sluiten uit.
    pub fn vfs<'a, B: Storage>(&'a mut self, b: &'a mut B) -> Result<Tracked<'a, B, LocalMarker>> {
        if self.closed {
            return Err(Error::State);
        }
        Ok(self.prepared.tracking.wrap(b, &mut self.prepared.marker))
    }
    /// Onveranderlijke marker voor status, archiefcatalogus en diagnose.
    pub fn marker(&self) -> &Marker {
        &self.prepared.marker.value
    }
    /// De laatste beurt, ook na sluiten.
    pub fn status(&self) -> Status {
        self.status
    }
    /// Zet de eigenaar permanent dicht. Actieve VFS-leningen moeten eerst eindigen;
    /// de host sluit SQLite en geeft daarna zijn lease vrij.
    pub fn close(&mut self) {
        self.closed = true;
    }
    /// Hosttimer-aanroep: vóór het interval geen I/O. Ook fouten krijgen een interval
    /// voordat de automatische volgende poging mag beginnen.
    pub fn tick<B: Storage, S: Store>(
        &mut self,
        b: &mut B,
        store: &mut S,
        now: Time,
    ) -> Result<Option<Synced>> {
        self.clock(now)?;
        if !self.due(now) {
            return Ok(None);
        }
        self.sync(b, store, now).map(Some)
    }
    fn due(&self, now: Time) -> bool {
        self.status
            .attempted
            .is_none_or(|last| now.seconds() - last.seconds() >= i64::from(self.config.interval))
    }
    fn clock(&self, now: Time) -> Result {
        if self.closed
            || now.seconds() < 1_577_836_800
            || self.status.attempted.is_some_and(|last| now < last)
        {
            return Err(Error::State);
        }
        Ok(())
    }
    /// Eén expliciete synchronisatie op de eigenaar: capture, upload en afronding
    /// in één beurt. Wie SQL wil laten doorlopen tijdens de upload van de delen
    /// gebruikt [`Replica::begin`] en [`Replica::finish`].
    pub fn sync<B: Storage, S: Store>(
        &mut self,
        b: &mut B,
        store: &mut S,
        now: Time,
    ) -> Result<Synced> {
        self.clock(now)?;
        if self.pending {
            return Err(Error::State);
        }
        self.status.attempted = Some(now);
        let result = match self.capture(b, store, now) {
            Ok(Some(pending)) => pending
                .upload(b, store)
                .and_then(|uploaded| self.complete(b, store, pending, uploaded, now)),
            Ok(None) => self.maintain(b, store, now),
            Err(e) => Err(e),
        };
        self.settle(result, now)
    }
    /// Alles vóór de upload van de delen: interval, onzekere commits, generatie
    /// en de capture. `None` als het interval nog loopt of als niets veranderde;
    /// in dat laatste geval is het onderhoud al gedaan. Tot [`Replica::finish`]
    /// schrijft SQL via [`Replica::vfs`] gewoon door, en is een tweede `begin`
    /// een fout.
    pub fn begin<B: Storage, S: Store>(
        &mut self,
        b: &mut B,
        store: &mut S,
        now: Time,
    ) -> Result<Option<Pending>> {
        self.clock(now)?;
        if self.pending {
            return Err(Error::State);
        }
        if !self.due(now) {
            return Ok(None);
        }
        self.status.attempted = Some(now);
        match self.capture(b, store, now) {
            Ok(Some(pending)) => {
                self.pending = true;
                Ok(Some(pending))
            }
            Ok(None) => {
                let result = self.maintain(b, store, now);
                self.settle(result, now).map(|_| None)
            }
            Err(e) => self.settle(Err(e), now).map(|_| None),
        }
    }
    /// Rondt een [`Replica::begin`] af met het resultaat van [`Pending::upload`]:
    /// manifest, marker, bevestiging en onderhoud. Een mislukte upload is de
    /// fout van deze beurt; de volgende capture neemt dezelfde pagina's mee.
    pub fn finish<B: Storage, S: Store>(
        &mut self,
        b: &mut B,
        store: &mut S,
        pending: Pending,
        uploaded: Result<Uploaded>,
        now: Time,
    ) -> Result<Synced> {
        self.pending = false;
        self.clock(now)?;
        let result = uploaded.and_then(|uploaded| self.complete(b, store, pending, uploaded, now));
        self.settle(result, now)
    }
    fn settle(&mut self, result: Result<Synced>, now: Time) -> Result<Synced> {
        self.status.error = result.as_ref().err().copied();
        if result.is_ok() {
            self.status.synced = Some(now);
        }
        result
    }
    fn invalidate<B: Storage>(&mut self, b: &mut B, damaged: bool) -> Result {
        let old = &self.prepared.marker.value;
        let mut marker = Marker::new(&self.config.destination, "", Time::ZERO)?;
        // Een onvoltooide repair blijft bewijzen welke gepubliceerde generatie van ons is.
        marker.repair_from = string(if damaged && old.complete {
            &old.generation
        } else {
            &old.repair_from
        })?;
        let mut local = LocalMarker::new(self.config.path, marker)?;
        // Eerst duurzaam besluiten; pas dan de onbetrouwbare trackingstaat loslaten.
        let tracking = Tracking::new(self.config.path, self.config.pages)?;
        if let Err(e) = local.save(b) {
            // De herstelbeslissing kan half op schijf staan; eerst opnieuw Prepare.
            self.closed = true;
            return Err(e);
        }
        self.prepared.marker = local;
        self.prepared.tracking = tracking;
        self.status.reason = if damaged {
            Reason::Repair
        } else {
            Reason::NewSnapshot
        };
        self.maintenance_at = None;
        Ok(())
    }
    fn archive_error<B: Storage>(&mut self, b: &mut B, error: Error) -> Error {
        if matches!(error, Error::Corrupt | Error::Gap | Error::Legacy)
            && let Err(e) = self.invalidate(b, true)
        {
            return e;
        }
        error
    }
    /// Onzekere commits oplossen, de generatie alleen op leeftijd vernieuwen,
    /// en de capture met zijn objectsleutels; nog geen upload.
    fn capture<B: Storage, S: Store>(
        &mut self,
        b: &mut B,
        store: &mut S,
        now: Time,
    ) -> Result<Option<Pending>> {
        if self.prepared.marker.value.uncertain != 0
            && let Err(e) =
                replication::resolve(b, store, &self.config.namespace, &mut self.prepared.marker)
        {
            return Err(self.archive_error(b, e));
        }
        let renew = self.prepared.marker.value.generation.is_empty()
            || (self.prepared.marker.value.complete
                && maintenance::renewal_due(
                    &self.prepared.marker.value,
                    now,
                    self.config.generation,
                )?);
        if renew {
            if self.prepared.marker.value.complete {
                self.status.reason = Reason::NewSnapshot;
            }
            replication::renew(b, &mut self.prepared.marker, now)?;
            self.maintenance_at = None;
        }
        let batch = match Batch::capture(
            b,
            self.config.path,
            &self.prepared.tracking,
            &self.prepared.marker,
            self.config.segment_bytes,
        ) {
            Ok(Some(batch)) => batch,
            Ok(None) => return Ok(None),
            Err(e @ (Error::ForeignWrite | Error::Gap | Error::State)) => {
                self.invalidate(b, false)?;
                return Err(e);
            }
            Err(e) => return Err(e),
        };
        let staged = batch.stage(b, &self.config.namespace, &self.prepared.marker, now)?;
        Ok(Some(Pending { batch, staged }))
    }
    /// Manifest en marker na de upload, daarna het onderhoud.
    fn complete<B: Storage, S: Store>(
        &mut self,
        b: &mut B,
        store: &mut S,
        pending: Pending,
        uploaded: Uploaded,
        now: Time,
    ) -> Result<Synced> {
        if let Err(e) = pending.batch.finish(
            b,
            store,
            uploaded,
            &mut self.prepared.marker,
            &mut self.prepared.tracking,
            now,
        ) {
            return Err(self.archive_error(b, e));
        }
        let mut result = self.maintain(b, store, now)?;
        result.published = true;
        Ok(result)
    }
    /// Onderhoud hoogstens eens per minuut; ook na een fout geen hotloop bij
    /// iedere SQL-wijziging.
    fn maintain<B: Storage, S: Store>(
        &mut self,
        b: &mut B,
        store: &mut S,
        now: Time,
    ) -> Result<Synced> {
        let mut result = Synced::default();
        if self.prepared.marker.value.complete
            && self
                .maintenance_at
                .is_none_or(|last| now.seconds() - last.seconds() >= 60)
        {
            self.maintenance_at = Some(now);
            result.maintenance = match maintenance::run(
                b,
                store,
                &self.config.namespace,
                &mut self.prepared.marker,
                &self.config.schedule,
                now,
                Budget {
                    pages: self.config.pages,
                    segment_bytes: self.config.segment_bytes,
                },
            ) {
                Ok(report) => report,
                Err(e) => return Err(self.archive_error(b, e)),
            };
            result.expired_objects = maintenance::generations(
                store,
                &self.config.namespace,
                &self.prepared.marker.value.generation,
                now,
                self.config.retention,
            )?;
        }
        Ok(result)
    }
}
