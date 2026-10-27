//! Cached diagnostics stay available while a consistent backup pauses SQLite writes.
use super::*;
impl<P: Persistence> Server<P> {
    /// Leest de opslag direct uit, los van de cadans van `maintain_storage`.
    pub(crate) fn refresh_storage(&mut self, now: &Timestamp) -> Result {
        let prunable = self.store.prunable_artifacts()?.len();
        self.refresh_storage_with(now, prunable)
    }
    pub(crate) fn refresh_storage_with(&mut self, now: &Timestamp, prunable: usize) -> Result {
        let time = now.time()?;
        let started = *self.storage_started.get_or_insert(time);
        let mut findings = d::List::new();
        let mut level = "ok";
        let mut add = |severity: &str, message: &str| -> Result {
            findings.push(http::object(&[
                ("level", Value::string(severity)?),
                ("message", Value::string(message)?),
            ])?)?;
            if severity == "error" {
                level = "error";
            } else if level == "ok" {
                level = "warning";
            }
            Ok(())
        };
        let (database_bytes, object_bytes, objects, replication, error) =
            match self.store.storage_usage() {
                Ok(Some(usage)) => (
                    usage.database_bytes,
                    usage.object_bytes,
                    usage.objects,
                    usage.replication,
                    "",
                ),
                Ok(None) => (0, 0, 0, Value::Null, ""),
                Err(_) => {
                    add("error", "De opslag kon niet worden uitgelezen.")?;
                    (0, 0, 0, Value::Null, "storage is unavailable")
                }
            };
        if let Some(replica) = replication.as_object() {
            // Elke week (en na een onzekere stop) begint Replica een nieuwe
            // generatie met een volledige kopie; tot die binnen is, is de
            // generatie niet compleet en synct er niets. Dat is gepland werk.
            let uploading = replica
                .get("generation")
                .and_then(Value::as_str)
                .and_then(generation_start)
                .map(|start| time.0.saturating_sub(start) / 60_000_000_000)
                .filter(|_| replica.get("complete") != Some(&Value::Bool(true)))
                .filter(|minutes| *minutes < FULL_UPLOAD_MINUTES);
            if let Some(minutes) = uploading {
                add(
                    "warning",
                    &spin_core::validation::text(format_args!(
                        "De wekelijkse volledige backup wordt geüpload (sinds {minutes} min, database {:.1} GiB); tot hij binnen is, synchroniseert de replica niet.",
                        database_bytes as f64 / (1u64 << 30) as f64
                    ))?,
                )?;
            } else if replica.get("complete") != Some(&Value::Bool(true)) {
                add("error", "Er is nog geen complete generatie in de replica.")?;
            }
            if replica
                .get("last_error")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            {
                add(
                    "error",
                    "De laatste synchronisatie van de replica is mislukt.",
                )?;
            }
            let synced = replica
                .get("last_sync_at")
                .filter(|v| !matches!(v, Value::Null))
                .map(Timestamp::from_value)
                .transpose()?
                .map(|t| t.time())
                .transpose()?;
            if uploading.is_none()
                && time.0.saturating_sub(synced.unwrap_or(started).0) > 300_000_000_000
            {
                add(
                    "error",
                    "De replica is al meer dan vijf minuten niet gesynchroniseerd.",
                )?;
            }
        } else {
            add(
                "warning",
                "Geen replica: deze Spin staat alleen op zijn eigen volume.",
            )?;
        }
        let report = http::object(&[
            ("database_bytes", Value::int(database_bytes)),
            ("object_bytes", Value::int(object_bytes)),
            ("objects", Value::int(objects)),
            ("prunable", Value::uint(prunable as u64)),
            ("replication", replication),
            ("error", Value::string(error)?),
            (
                "health",
                http::object(&[
                    ("level", Value::string(level)?),
                    ("findings", findings.to_value()?),
                ])?,
            ),
        ])?;
        if self.storage_report != report {
            self.storage_report = report;
            self.display_changed();
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn prune_snapshot(&mut self, now: &Timestamp) -> Result {
        let prunable = self.store.prunable_artifacts()?;
        self.prune_first(prunable.first(), now)
    }
    /// Ruimt per ronde hoogstens één snapshot op; de volgende wacht op de volgende ronde.
    pub(crate) fn prune_first(
        &mut self,
        artifact: Option<&d::Artifact>,
        now: &Timestamp,
    ) -> Result {
        if let Some(artifact) = artifact {
            let reference =
                spin_core::validation::text(format_args!("snapshot:{}", artifact.snapshot.digest))?;
            match self.store.blob(spin_store::BlobRequest::Delete(&reference)) {
                Ok(_) | Err(spin_store::Error::NotFound) => {
                    self.store.mark_snapshot_pruned(&artifact.id, now)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

/// Zo lang mag een volledige kopie duren voordat een onvolledige generatie een fout is.
const FULL_UPLOAD_MINUTES: u64 = 3 * 60;
/// De starttijd uit een generatienaam (`20261027T134625Z-<id>`), in nanoseconden.
fn generation_start(name: &str) -> Option<u64> {
    let s = name.get(..16).filter(|s| s.ends_with('Z'))?;
    let iso = alloc::format!(
        "\"{}-{}-{}T{}:{}:{}Z\"",
        s.get(0..4)?,
        s.get(4..6)?,
        s.get(6..8)?,
        s.get(9..11)?,
        s.get(11..13)?,
        s.get(13..15)?
    );
    Timestamp::from_json(iso.as_bytes()).ok()?.time().ok().map(|t| t.0)
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Uploading;
    impl Persistence for Uploading {
        fn save(&mut self, _: &PersistedState) -> spin_store::Result {
            Ok(())
        }
        fn storage_usage(&mut self) -> spin_store::Result<Option<spin_store::StorageUsage>> {
            Ok(Some(spin_store::StorageUsage {
                database_bytes: 17 << 30,
                object_bytes: 0,
                objects: 0,
                replication: Value::from_json(
                    br#"{"generation":"20261027T134625Z-856d","complete":false,"last_sync_at":null,"last_error":""}"#,
                )
                .unwrap(),
            }))
        }
    }
    #[test]
    fn a_running_full_backup_is_a_warning_until_it_takes_too_long() {
        assert_eq!(generation_start("current"), None);
        let mut server = Server::new(Store::new(PersistedState::default(), Uploading));
        let level = |server: &mut Server<Uploading>, at: &str| {
            let now = Timestamp::from_json(alloc::format!("\"{at}\"").as_bytes()).unwrap();
            server.refresh_storage(&now).unwrap();
            server.storage_report.to_json().unwrap()
        };
        let report = level(&mut server, "2026-10-27T14:30:00Z");
        assert!(report.contains("\"level\":\"warning\""), "{report}");
        assert!(report.contains("sinds 43 min"), "{report}");
        assert!(!report.contains("niet gesynchroniseerd."), "{report}");
        let report = level(&mut server, "2026-10-27T17:00:00Z");
        assert!(report.contains("Er is nog geen complete generatie"), "{report}");
        assert!(report.contains("niet gesynchroniseerd."), "{report}");
    }
    use core::cell::Cell;
    use d::state::PersistedState;
    struct Counting<'a>(&'a Cell<u32>);
    impl Persistence for Counting<'_> {
        fn save(&mut self, _: &PersistedState) -> spin_store::Result {
            Ok(())
        }
        fn storage_usage(&mut self) -> spin_store::Result<Option<spin_store::StorageUsage>> {
            self.0.set(self.0.get() + 1);
            Ok(None)
        }
    }
    #[test]
    fn maintenance_reads_storage_usage_at_most_every_thirty_seconds() {
        let reads = Cell::new(0);
        let mut server = Server::new(Store::new(PersistedState::default(), Counting(&reads)));
        let at = |second: &str| {
            Timestamp::from_json(alloc::format!("\"2026-09-30T12:00:{second}Z\"").as_bytes())
                .unwrap()
        };
        for second in ["00", "01", "02", "29"] {
            server.maintain_storage(&at(second)).unwrap();
        }
        assert_eq!(reads.get(), 1);
        assert!(!server.storage_report.is_null());
        server.maintain_storage(&at("30")).unwrap();
        assert_eq!(reads.get(), 2);
        // Een directe uitlezing blijft altijd vers.
        server.refresh_storage(&at("31")).unwrap();
        assert_eq!(reads.get(), 3);
    }
}
