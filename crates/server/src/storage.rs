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
            if replica.get("complete") != Some(&Value::Bool(true)) {
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
            if time.0.saturating_sub(synced.unwrap_or(started).0) > 300_000_000_000 {
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

#[cfg(test)]
mod tests {
    use super::*;
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
