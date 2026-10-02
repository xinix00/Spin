//! Browsergebonden tickets en één consistente, begrensde database-download.
use super::*;
use spin_core::{backup::Zip, validation::text};
use spin_store::backup::{Reply, Request as StorageRequest};
pub(crate) struct Ticket {
    owner: String,
    expires: u64,
}
pub(crate) struct Export {
    id: String,
    zip: Zip,
    expires: u64,
}
/// Headers en streamhandvat voor de netwerk-runtime.
pub struct Download {
    /// Cachevrije downloadheaders.
    pub response: Response,
    /// Alleen dit handvat mag de export lezen en vrijgeven.
    pub wait: BackupWait,
}
/// Verwijst uitsluitend naar de export die deze caller heeft geopend.
pub struct BackupWait {
    id: String,
    complete: core::cell::Cell<bool>,
}
impl<P: Persistence> Server<P> {
    /// Tijdens de consistente download pauzeert de actor zijn muterende achtergrondwerk.
    pub fn backup_active(&self) -> bool {
        self.export.is_some() || self.restores.iter().any(|r| !r.done)
    }
    pub(crate) fn backup_route(
        &mut self,
        req: &Request<'_>,
        user: &d::User,
        owner: &str,
        now: &Timestamp,
        random: &mut impl Runtime,
    ) -> Result<Option<Outcome>> {
        if !matches!(
            (req.method, req.path),
            ("POST", "/api/backup-ticket" | "/api/backup") | ("GET", "/api/backup")
        ) {
            return Ok(None);
        }
        if user.role != d::USER_ADMIN {
            return Err(Error::Http(403, "admin role required"));
        }
        let current = now.time()?.0;
        self.backup_tickets.retain(|_, t| t.expires > current);
        if req.path == "/api/backup-ticket" {
            if self.backup_tickets.len() >= 32 {
                return Err(Error::Http(503, "backup ticket capacity reached"));
            }
            let token = auth::token(random)?;
            let response = Response::json(
                201,
                &http::object(&[(
                    "url",
                    Value::string(&text(format_args!("/api/backup?ticket={token}"))?)?,
                )])?,
            )?;
            self.backup_tickets.insert(
                spin_security::digest_hex(token.as_bytes())?,
                Ticket {
                    owner: d::try_string(owner)?,
                    expires: current.saturating_add(60_000_000_000),
                },
            )?;
            return Ok(Some(Outcome::Response(response)));
        }
        if req.method == "GET" {
            let key = spin_security::digest_hex(req.query("ticket")?.as_bytes())?;
            if !self
                .backup_tickets
                .get(&key)
                .is_some_and(|t| t.owner == owner && t.expires > current)
            {
                return Err(Error::Http(403, "backup ticket invalid or expired"));
            }
            self.backup_tickets.remove(&key);
        }
        if self.export.is_some() {
            return Err(Error::Http(503, "a backup is already streaming"));
        }
        let id = random.next("backup")?;
        let wait = BackupWait {
            id: id.try_clone()?,
            complete: core::cell::Cell::new(false),
        };
        let Reply::Ready { size, key } = self.store.backup(StorageRequest::Begin)? else {
            return Err(Error::Http(500, "invalid backup storage response"));
        };
        let prepared = (|| -> Result<_> {
            let zip = Zip::new(size, &key)?;
            let mut response = Response::empty(200)?;
            response.header("Content-Type", "application/zip")?;
            response.header(
                "Content-Disposition",
                "attachment; filename=\"spin-backup.zip\"",
            )?;
            response.header("X-Spin-Backup-Contains-Secrets", "true")?;
            response.header("Content-Length", &text(format_args!("{}", zip.size()))?)?;
            Ok((zip, response))
        })();
        match prepared {
            Ok((zip, response)) => {
                self.export = Some(Export {
                    id,
                    zip,
                    expires: current.saturating_add(6 * 3600 * 1_000_000_000),
                });
                Ok(Some(Outcome::Download(Download { response, wait })))
            }
            Err(error) => {
                self.store.backup(StorageRequest::End)?;
                Err(error)
            }
        }
    }
    /// Leest pas een blok wanneer de socket het vorige heeft verstuurd.
    pub fn backup_chunk(
        &mut self,
        wait: &BackupWait,
        now: &Timestamp,
    ) -> Result<Option<alloc::vec::Vec<u8>>> {
        if wait.complete.get() {
            return Ok(None);
        }
        let export = self
            .export
            .as_mut()
            .filter(|e| e.id == wait.id)
            .ok_or(Error::Http(409, "backup no longer active"))?;
        if export.expires <= now.time()?.0 {
            return Err(Error::Http(408, "backup deadline reached"));
        }
        let bytes = if let Some((offset, length)) = export.zip.read_range() {
            let Reply::Bytes(bytes) = self.store.backup(StorageRequest::Read { offset, length })?
            else {
                return Err(Error::Http(500, "invalid backup read"));
            };
            Some(bytes)
        } else {
            None
        };
        let chunk = export.zip.next(bytes)?;
        if export.zip.finished() {
            self.finish_backup(wait)?;
        }
        Ok(chunk)
    }
    /// Ook een afgebroken download geeft writes vrij; een oud handvat doet niets.
    pub fn finish_backup(&mut self, wait: &BackupWait) -> Result {
        if self.export.as_ref().is_some_and(|e| e.id == wait.id) {
            self.store.backup(StorageRequest::End)?;
            self.export = None;
        }
        wait.complete.set(true);
        Ok(())
    }
}
