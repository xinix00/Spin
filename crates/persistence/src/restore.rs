//! A staged backup is data: never copy its schema, triggers, or views into the live database.
use super::*;
impl<'e, 'a, B: Storage> Database<'e, 'a, B> {
    /// Read-only access to a previously validated immutable staging file.
    pub fn read_only(mut connection: Connection<'e, 'a, B>) -> Result<Self> {
        connection.execute(c"PRAGMA trusted_schema=OFF; PRAGMA query_only=ON;")?;
        Ok(Self {
            connection,
            uncertain: None,
        })
    }
    /// Open an already existing staged database without creating or deleting tables.
    pub fn staged(connection: Connection<'e, 'a, B>) -> Result<Self> {
        let mut db = Self::read_only(connection)?;
        db.quick_check()?;
        {
            let mut s=db.connection.prepare(c"SELECT count(*) FROM sqlite_schema WHERE type='table' AND name IN ('spin_kv','spin_objects','spin_object_chunks','spin_object_refs')")?;
            if !s.step()? || integer(&mut s, 0)? != 4 {
                return Err(Error::Invalid("backup schema is incomplete"));
            }
        }
        Ok(db)
    }
    /// List complete object references for incremental hash validation.
    pub fn restore_object(&mut self, after: &str) -> Result<Option<BlobInfo>> {
        let mut s=self.connection.prepare(c"SELECT r.ref,o.digest,o.kind,o.size FROM spin_object_refs r JOIN spin_objects o ON o.id=r.object_id WHERE r.ref>? AND o.complete=1 ORDER BY r.ref LIMIT 1")?;
        s.bind(1, Value::Text(after))?;
        if !s.step()? {
            return Ok(None);
        }
        Ok(Some(BlobInfo {
            reference: string(&mut s, 0)?,
            digest: string(&mut s, 1)?,
            kind: string(&mut s, 2)?,
            size: integer(&mut s, 3)?,
        }))
    }
    /// Import only known data columns. The destination rollback journal protects the entire replacement.
    pub fn install_restore(&mut self, rows: &[Row]) -> Result {
        self.connection.execute(
            c"PRAGMA trusted_schema=OFF; ATTACH DATABASE 'spin-restore.sqlite' AS incoming;",
        )?;
        let result=self.transaction(|db| {
            db.connection.execute(c"DELETE FROM main.spin_object_refs; DELETE FROM main.spin_object_chunks; DELETE FROM main.spin_objects; DELETE FROM main.spin_kv;
INSERT INTO main.spin_objects(id,digest,kind,size,complete) SELECT id,digest,kind,size,complete FROM incoming.spin_objects WHERE complete=1;
INSERT INTO main.spin_object_chunks(object_id,sequence,data) SELECT c.object_id,c.sequence,c.data FROM incoming.spin_object_chunks c JOIN main.spin_objects o ON o.id=c.object_id;
INSERT INTO main.spin_object_refs(ref,object_id) SELECT r.ref,r.object_id FROM incoming.spin_object_refs r JOIN main.spin_objects o ON o.id=r.object_id;
INSERT INTO main.spin_kv(key,value) SELECT key,value FROM incoming.spin_kv WHERE key NOT IN ('state','backup/format','backup/master_key');
DELETE FROM main.spin_rows;")?;
            db.put_rows(rows)
        });
        // A failed detach cannot undo a confirmed commit; closing this epoch releases the attachment.
        let _ = self.connection.execute(c"DETACH DATABASE incoming");
        result
    }
}
