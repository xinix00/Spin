//! Onzichtbare objecten ontvangen één duurzame chunk per actoropdracht.
use super::*;
use spin_store::{BlobReply, BlobRequest};
fn info(value: BlobInfo) -> spin_store::BlobInfo {
    spin_store::BlobInfo {
        reference: value.reference,
        digest: value.digest,
        kind: value.kind,
        size: value.size,
    }
}
pub(super) fn store_error(error: Error) -> spin_store::Error {
    match error {
        Error::Sql(e) => spin_store::Error::Storage(e.code),
        Error::Uncertain(code) => spin_store::Error::StorageUncertain(code),
        Error::Data(e) => spin_store::Error::Data(e),
        Error::Security(e) => spin_store::Error::Security(e),
        Error::NotFound => spin_store::Error::NotFound,
        Error::Invalid(reason) => spin_store::Error::Conflict(reason),
    }
}
impl<B: Storage> Database<'_, '_, B> {
    fn pending_info(&mut self, object: i64) -> Result<(String, i64)> {
        self.ready()?;
        let mut statement = self
            .connection
            .prepare(c"SELECT kind,size FROM spin_objects WHERE id=? AND complete=0")?;
        statement.bind(1, Value::Integer(object))?;
        if !statement.step()? {
            return Err(Error::NotFound);
        }
        Ok((string(&mut statement, 0)?, integer(&mut statement, 1)?))
    }
    /// De publieke dispatcher houdt opslagtypes buiten de server- en Store-crates.
    pub fn blob(&mut self, request: BlobRequest<'_>) -> Result<BlobReply> {
        match request {
            BlobRequest::Begin { kind, size } => {
                if kind.is_empty() || kind.len() > 64 || !(1..=64_i64 << 30).contains(&size) {
                    return Err(Error::Invalid("invalid upload kind or size"));
                }
                self.transaction(|db| {
                    db.execute(
                        c"INSERT INTO spin_objects(kind,size) VALUES(?,?)",
                        &[Value::Text(kind), Value::Integer(size)],
                    )?;
                    let mut statement = db.connection.prepare(c"SELECT last_insert_rowid()")?;
                    if !statement.step()? {
                        return Err(Error::Invalid("missing upload ID"));
                    }
                    Ok(BlobReply::Upload(integer(&mut statement, 0)?))
                })
            }
            BlobRequest::Write {
                object,
                offset,
                bytes,
            } => {
                let (_, size) = self.pending_info(object)?;
                let length =
                    i64::try_from(bytes.len()).map_err(|_| Error::Invalid("chunk size"))?;
                let unit = BLOB_CHUNK_SIZE as i64;
                if offset < 0
                    || offset % unit != 0
                    || bytes.is_empty()
                    || bytes.len() > BLOB_CHUNK_SIZE
                    || offset
                        .checked_add(length)
                        .is_none_or(|end| end > size || (length != unit && end != size))
                {
                    return Err(Error::Invalid(
                        "upload chunks must be aligned and full except the last",
                    ));
                }
                self.transaction(|db| db.execute(c"INSERT INTO spin_object_chunks(object_id,sequence,data) VALUES(?,?,?) ON CONFLICT(object_id,sequence) DO UPDATE SET data=excluded.data", &[Value::Integer(object), Value::Integer(offset / unit), Value::Blob(bytes)]))?;
                Ok(BlobReply::Done)
            }
            BlobRequest::Pending { object, offset } => {
                let (_, size) = self.pending_info(object)?;
                let unit = BLOB_CHUNK_SIZE as i64;
                if offset < 0 || offset >= size || offset % unit != 0 {
                    return Err(Error::Invalid("invalid upload read offset"));
                }
                let mut statement = self.connection.prepare(
                    c"SELECT data FROM spin_object_chunks WHERE object_id=? AND sequence=?",
                )?;
                statement.bind(1, Value::Integer(object))?;
                statement.bind(2, Value::Integer(offset / unit))?;
                if !statement.step()? {
                    return Err(Error::NotFound);
                }
                match statement.column(0)? {
                    Value::Blob(bytes) if bytes.len() as i64 == (size - offset).min(unit) => {
                        Ok(BlobReply::Bytes(copy(bytes)?))
                    }
                    _ => Err(Error::Invalid("invalid upload chunk length")),
                }
            }
            BlobRequest::Publish {
                object,
                reference,
                digest,
            } => {
                if reference.is_empty()
                    || reference.len() > 512
                    || digest.len() != 71
                    || !digest.starts_with("sha256:")
                    || !digest.as_bytes()[7..].iter().all(u8::is_ascii_hexdigit)
                {
                    return Err(Error::Invalid("invalid upload publication"));
                }
                let (kind, size) = self.pending_info(object)?;
                self.transaction(|db| {
                    {
                        let mut s = db.connection.prepare(c"SELECT COUNT(*),COALESCE(SUM(length(data)),0),COALESCE(MIN(sequence),-1),COALESCE(MAX(sequence),-1) FROM spin_object_chunks WHERE object_id=?")?;
                        s.bind(1, Value::Integer(object))?;
                        let count = (size + BLOB_CHUNK_SIZE as i64 - 1) / BLOB_CHUNK_SIZE as i64;
                        if !s.step()? || integer(&mut s,0)? != count || integer(&mut s,1)? != size || integer(&mut s,2)? != 0 || integer(&mut s,3)? != count-1 { return Err(Error::Invalid("upload is incomplete")); }
                    }
                    let existing = {
                        let mut s = db.connection.prepare(c"SELECT id FROM spin_objects WHERE digest=? AND complete=1")?;
                        s.bind(1, Value::Text(digest))?;
                        if s.step()? { Some(integer(&mut s,0)?) } else { None }
                    };
                    let id = if let Some(id) = existing { db.execute(c"UPDATE spin_objects SET complete=-1 WHERE id=? AND complete=0", &[Value::Integer(object)])?; id }
                    else { db.execute(c"UPDATE spin_objects SET digest=?,complete=1 WHERE id=? AND complete=0", &[Value::Text(digest),Value::Integer(object)])?; object };
                    db.execute(c"INSERT INTO spin_object_refs(ref,object_id) VALUES(?,?) ON CONFLICT(ref) DO UPDATE SET object_id=excluded.object_id", &[Value::Text(reference),Value::Integer(id)])?;
                    Ok(BlobReply::Info(spin_store::BlobInfo { reference: try_string(reference)?, digest: try_string(digest)?, kind, size }))
                })
            }
            BlobRequest::Abandon(object) => {
                self.transaction(|db| {
                    db.execute(
                        c"UPDATE spin_objects SET complete=-1 WHERE id=? AND complete=0",
                        &[Value::Integer(object)],
                    )
                })?;
                Ok(BlobReply::Done)
            }
            BlobRequest::Info(reference) => Ok(BlobReply::Info(info(self.blob_info(reference)?))),
            BlobRequest::Chunk { reference, offset } => {
                let (bytes, metadata) = self.read_blob_chunk(reference, offset)?;
                Ok(BlobReply::Chunk(bytes, info(metadata)))
            }
            BlobRequest::Put {
                reference,
                kind,
                mut bytes,
            } => {
                let metadata = self.put_blob(reference, kind, |buffer| {
                    let count = bytes.len().min(buffer.len());
                    buffer[..count].copy_from_slice(&bytes[..count]);
                    bytes = &bytes[count..];
                    Ok(count)
                })?;
                Ok(BlobReply::Info(info(metadata)))
            }
            BlobRequest::Delete(reference) => {
                self.delete_blob(reference)?;
                Ok(BlobReply::Done)
            }
        }
    }
}
