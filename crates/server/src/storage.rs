//! Cached diagnostics stay available while a consistent backup pauses SQLite writes.
use super::*;
impl<P: Persistence> Server<P> {
    pub(crate) fn refresh_storage(&mut self, now: &Timestamp) -> Result {
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
            (
                "prunable",
                Value::uint(self.store.prunable_artifacts()?.len() as u64),
            ),
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
    pub(crate) fn prune_snapshot(&mut self, now: &Timestamp) -> Result {
        if let Some(artifact) = self.store.prunable_artifacts()?.iter().next() {
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
