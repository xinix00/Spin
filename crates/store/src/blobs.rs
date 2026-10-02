//! Blobopdrachten blijven bij dezelfde eigenaar als de toestandopslag.
use crate::{Error, Persistence, Result, Store};
use alloc::{string::String, vec::Vec};
/// Metadata van een volledig gepubliceerd object.
pub struct BlobInfo {
    /// Logische referentie, bijvoorbeeld snapshot:sha256:… of bundle:….
    pub reference: String,
    /// SHA-256 van de archiefbytes.
    pub digest: String,
    /// Soort object.
    pub kind: String,
    /// Aantal bytes.
    pub size: i64,
}
/// Eén begrensde opdracht aan de blobopslag.
pub enum BlobRequest<'a> {
    /// Begin een nog onzichtbaar object.
    Begin {
        /// Soort object.
        kind: &'a str,
        /// Verwachte omvang.
        size: i64,
    },
    /// Schrijf een uitgelijnde chunk; de laatste mag korter zijn.
    Write {
        /// Onzichtbare object-ID.
        object: i64,
        /// Byteoffset.
        offset: i64,
        /// Hoogstens 1 MiB.
        bytes: &'a [u8],
    },
    /// Lees één chunk van een upload voor incrementele verificatie.
    Pending {
        /// Object-ID.
        object: i64,
        /// Byteoffset.
        offset: i64,
    },
    /// Publiceer pas nadat de eigenaar alle chunks geverifieerd heeft.
    Publish {
        /// Object-ID.
        object: i64,
        /// Logische referentie.
        reference: &'a str,
        /// Gecontroleerde SHA-256.
        digest: &'a str,
    },
    /// Verwijder een niet gepubliceerde upload.
    Abandon(i64),
    /// Lees metadata van een gepubliceerd object.
    Info(&'a str),
    /// Lees één uitgelijnde chunk plus metadata.
    Chunk {
        /// Logische referentie.
        reference: &'a str,
        /// Byteoffset.
        offset: i64,
    },
    /// Sla een klein object in één transactie op.
    Put {
        /// Logische referentie.
        reference: &'a str,
        /// Soort object.
        kind: &'a str,
        /// Begrensde bytes.
        bytes: &'a [u8],
    },
    /// Verwijder een referentie en ruim ongebruikte inhoud op.
    Delete(&'a str),
}
/// Het resultaat van één blobopdracht.
pub enum BlobReply {
    /// Bevestigde mutatie.
    Done,
    /// Een nieuwe, onzichtbare upload.
    Upload(i64),
    /// Objectmetadata.
    Info(BlobInfo),
    /// Uploadchunk.
    Bytes(Vec<u8>),
    /// Downloadchunk met objectmetadata.
    Chunk(Vec<u8>, BlobInfo),
}
impl<P: Persistence> Store<P> {
    /// Voert blob-I/O onder dezelfde duurzame foutgrens als toestandmutaties uit.
    pub fn blob(&mut self, request: BlobRequest<'_>) -> Result<BlobReply> {
        if let Some(code) = self.uncertain {
            return Err(Error::StorageUncertain(code));
        }
        let result = self.persistence.blob(request);
        if let Err(Error::StorageUncertain(code)) = &result {
            self.uncertain = Some(*code);
        }
        result
    }
}

impl<P: Persistence> Store<P> {
    /// Kleine blobs hebben een expliciet bytebudget; snapshots blijven chunkgewijs.
    pub fn read_blob(&mut self, reference: &str, limit: usize) -> Result<Vec<u8>> {
        let BlobReply::Info(info) = self.blob(BlobRequest::Info(reference))? else {
            return Err(Error::Conflict("invalid blob metadata"));
        };
        let size = usize::try_from(info.size).map_err(|_| Error::Conflict("invalid blob size"))?;
        if size > limit {
            return Err(Error::Conflict("blob exceeds byte budget"));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| spin_domain::Error::OutOfMemory)?;
        while bytes.len() < size {
            let BlobReply::Chunk(chunk, current) = self.blob(BlobRequest::Chunk {
                reference,
                offset: bytes.len() as i64,
            })?
            else {
                return Err(Error::Conflict("invalid blob chunk"));
            };
            if current.digest != info.digest
                || current.size != info.size
                || chunk.is_empty()
                || chunk.len() > size - bytes.len()
            {
                return Err(Error::Conflict("blob changed during read"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

/// A bounded outbox survives a failed object deletion or a server restart.
pub(crate) fn queue_garbage(
    state: &mut spin_domain::state::PersistedState,
    reference: &str,
) -> Result {
    use spin_domain::try_string;
    if state.garbage_refs.iter().any(|r| r == reference) {
        return Ok(());
    }
    if state.garbage_refs.len() >= 65536 {
        return Err(Error::Conflict("blob cleanup queue is full"));
    }
    state.garbage_refs.push(try_string(reference)?)?;
    Ok(())
}
impl<P: Persistence> Store<P> {
    /// Remove at most one unreferenced object, then acknowledge its cleanup transaction.
    pub fn collect_blob_garbage(&mut self) -> Result {
        use spin_domain::TryClone;
        let Some(reference) = self.state.garbage_refs.iter().next() else {
            return Ok(());
        };
        let live = if let Some(digest) = reference.strip_prefix("snapshot:") {
            self.state
                .artifacts
                .iter()
                .any(|(_, a)| a.snapshot.digest == digest && a.snapshot_pruned_at.is_none())
        } else if let Some(id) = reference.strip_prefix("attachment:") {
            self.state.job_attachments.get(id).is_some()
        } else if let Some(id) = reference.strip_prefix("manifest:artifact:") {
            self.state.artifacts.get(id).is_some()
        } else if let Some(id) = reference.strip_prefix("manifest:composition:") {
            self.state.compositions.get(id).is_some()
        } else {
            self.state
                .deliverables
                .iter()
                .any(|(_, d)| d.bundle.as_ref().is_some_and(|b| b.r#ref == *reference))
        };
        let reference = reference.try_clone()?;
        if !live {
            match self.blob(BlobRequest::Delete(&reference)) {
                Ok(_) | Err(Error::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
        self.edit(|state| {
            state.garbage_refs.retain(|r| r != &reference);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};
    use spin_domain::{TryClone, Wire, state::PersistedState};
    struct Saved<'a> {
        state: &'a RefCell<PersistedState>,
        fail: &'a Cell<bool>,
        deletes: &'a RefCell<Vec<String>>,
    }
    impl Persistence for Saved<'_> {
        fn save(&mut self, state: &PersistedState) -> Result {
            *self.state.borrow_mut() = state.try_clone()?;
            Ok(())
        }
        fn blob(&mut self, request: BlobRequest<'_>) -> Result<BlobReply> {
            let BlobRequest::Delete(reference) = request else {
                return Err(Error::NotFound);
            };
            if self.fail.get() {
                return Err(Error::Storage(10));
            }
            self.deletes
                .borrow_mut()
                .push(spin_domain::try_string(reference)?);
            Ok(BlobReply::Done)
        }
    }
    #[test]
    fn deletion_cleanup_survives_restart_and_never_deletes_a_reused_reference() {
        let state=PersistedState::from_json(br#"{"jobs":{"j":{"id":"j","owner":"derek"}},"job_attachments":{"kept":{"id":"kept","job_id":"other"}}}"#).unwrap();
        let saved = RefCell::new(state.try_clone().unwrap());
        let failed = Cell::new(true);
        let deleted = RefCell::new(Vec::new());
        let persistence = || Saved {
            state: &saved,
            fail: &failed,
            deletes: &deleted,
        };
        let mut store = Store::new(state, persistence());
        store
            .delete_job_with_blobs(
                "j",
                "derek",
                &[
                    String::from("attachment:gone"),
                    String::from("attachment:kept"),
                ],
            )
            .unwrap();
        assert!(store.job("j").is_err());
        assert!(store.collect_blob_garbage().is_err());
        assert_eq!(saved.borrow().garbage_refs.len(), 2);
        drop(store);
        let state = saved.borrow().try_clone().unwrap();
        let mut restarted = Store::new(state, persistence());
        failed.set(false);
        restarted.collect_blob_garbage().unwrap();
        restarted.collect_blob_garbage().unwrap();
        assert!(saved.borrow().garbage_refs.is_empty());
        assert_eq!(
            deleted.borrow().as_slice(),
            [String::from("attachment:gone")]
        );
    }
}
