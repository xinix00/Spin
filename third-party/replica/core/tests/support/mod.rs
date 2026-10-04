#![allow(dead_code, clippy::unwrap_used)]
use replica_core::object::{Object, Store, StoreError};
use replica_sqlite::{Error, FileId, OpenFlags, Result, Storage};
use std::{collections::BTreeMap, ffi::CStr};
#[derive(Clone, Default)]
pub(crate) struct Bucket {
    pub data: BTreeMap<String, Vec<u8>>,
    pub put_reply_lost: bool,
    pub fail_put_match: Option<String>,
    pub fail_before_put: bool,
    pub deleted: Vec<String>,
    pub fail_delete: Option<usize>,
    pub get_error: Option<StoreError>,
}
impl Store for Bucket {
    fn put(&mut self, key: &str, bytes: &[u8]) -> core::result::Result<(), StoreError> {
        let fail = self.put_reply_lost
            || self
                .fail_put_match
                .as_ref()
                .is_some_and(|s| key.contains(s));
        if fail && self.fail_before_put {
            return Err(StoreError::Transport);
        }
        self.data.insert(key.into(), bytes.to_vec());
        if fail {
            Err(StoreError::Transport)
        } else {
            Ok(())
        }
    }
    fn get(&mut self, key: &str, limit: usize) -> core::result::Result<Vec<u8>, StoreError> {
        if let Some(e) = self.get_error {
            return Err(e);
        }
        let b = self.data.get(key).ok_or(StoreError::Missing)?;
        if b.len() > limit {
            return Err(StoreError::Limit);
        }
        Ok(b.clone())
    }
    fn delete(&mut self, key: &str) -> core::result::Result<(), StoreError> {
        self.deleted.push(key.into());
        if self.fail_delete == Some(self.deleted.len()) {
            return Err(StoreError::Transport);
        }
        self.data.remove(key);
        Ok(())
    }
    fn list(
        &mut self,
        prefix: &str,
        limit: usize,
    ) -> core::result::Result<Vec<Object>, StoreError> {
        let v: Vec<_> = self
            .data
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| Object {
                key: k.clone(),
                size: Some(v.len() as u64),
            })
            .collect();
        if v.len() > limit {
            return Err(StoreError::Limit);
        }
        Ok(v)
    }
}
#[derive(Clone, Default)]
pub(crate) struct Fs {
    pub files: Vec<Entry>,
    pub stable: BTreeMap<String, Vec<u8>>,
    pub events: Vec<(String, &'static str)>,
    pub fail: Option<(usize, bool)>,
    pub ops: usize,
    pub random_calls: u8,
}
#[derive(Clone)]
pub(crate) struct Entry {
    pub name: String,
    pub data: Vec<u8>,
    pub open: bool,
}
impl Fs {
    fn begin(&mut self, name: &str, op: &'static str) -> Result<bool> {
        self.ops += 1;
        self.events.push((name.into(), op));
        if let Some((n, after)) = self.fail
            && self.ops == n
        {
            if !after {
                return Err(Error::IO);
            }
            return Ok(true);
        }
        Ok(false)
    }
    fn finish(fail: bool) -> Result {
        if fail { Err(Error::IO) } else { Ok(()) }
    }
    fn index(&self, file: FileId) -> Result<usize> {
        let i = file.0 as usize;
        if self.files.get(i).is_none_or(|e| !e.open) {
            Err(Error::IO)
        } else {
            Ok(i)
        }
    }
    pub(crate) fn data(&self, name: &str) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.data.as_slice())
    }
    pub(crate) fn cold(&self) -> Self {
        Self {
            files: self
                .stable
                .iter()
                .map(|(name, data)| Entry {
                    name: name.clone(),
                    data: data.clone(),
                    open: false,
                })
                .collect(),
            stable: self.stable.clone(),
            ..Self::default()
        }
    }
}
impl Storage for Fs {
    fn open(&mut self, name: &CStr, flags: OpenFlags) -> Result<FileId> {
        let name = name.to_str().map_err(|_| Error::TEXT)?;
        if let Some(i) = self.files.iter().position(|e| e.name == name) {
            if self.files[i].open {
                return Err(Error { code: 5 });
            }
            self.files[i].open = true;
            return Ok(FileId(i as u32));
        }
        if !flags.is_create() {
            return Err(Error::CANNOT_OPEN);
        }
        let i = self.files.len();
        self.files.push(Entry {
            name: name.into(),
            data: Vec::new(),
            open: true,
        });
        Ok(FileId(i as u32))
    }
    fn close(&mut self, file: FileId) -> Result {
        let i = self.index(file)?;
        self.files[i].open = false;
        Ok(())
    }
    fn read(&mut self, file: FileId, offset: u64, dst: &mut [u8]) -> Result<usize> {
        let i = self.index(file)?;
        let off = usize::try_from(offset).map_err(|_| Error::IO)?;
        let n = self.files[i].data.len().saturating_sub(off).min(dst.len());
        if n > 0 {
            dst[..n].copy_from_slice(&self.files[i].data[off..off + n]);
        }
        Ok(n)
    }
    fn write(&mut self, file: FileId, offset: u64, src: &[u8]) -> Result {
        let i = self.index(file)?;
        let name = self.files[i].name.clone();
        let fail = self.begin(&name, "write")?;
        let off = offset as usize;
        let end = off
            .checked_add(src.len())
            .filter(|n| *n < 16 << 20)
            .ok_or(Error::FULL)?;
        let data = &mut self.files[i].data;
        data.resize(data.len().max(end), 0);
        data[off..end].copy_from_slice(src);
        Self::finish(fail)
    }
    fn truncate(&mut self, file: FileId, size: u64) -> Result {
        let i = self.index(file)?;
        let name = self.files[i].name.clone();
        let fail = self.begin(&name, "truncate")?;
        if size > 16 << 20 {
            return Err(Error::FULL);
        }
        self.files[i].data.resize(size as usize, 0);
        Self::finish(fail)
    }
    fn sync(&mut self, file: FileId, _: i32) -> Result {
        let i = self.index(file)?;
        let name = self.files[i].name.clone();
        let fail = self.begin(&name, "sync")?;
        self.stable.insert(name, self.files[i].data.clone());
        Self::finish(fail)
    }
    fn size(&mut self, file: FileId) -> Result<u64> {
        let i = self.index(file)?;
        Ok(self.files[i].data.len() as u64)
    }
    fn remove(&mut self, name: &CStr, dir: bool) -> Result {
        let name = name.to_str().map_err(|_| Error::TEXT)?;
        let fail = self.begin(name, "remove")?;
        if let Some(e) = self.files.iter_mut().find(|e| e.name == name) {
            if e.open {
                return Err(Error { code: 5 });
            }
            e.name.clear();
            e.data.clear();
        }
        if dir {
            self.stable.remove(name);
        }
        Self::finish(fail)
    }
    fn exists(&mut self, name: &CStr) -> Result<bool> {
        Ok(self.files.iter().any(|e| e.name == name.to_str().unwrap()))
    }
    fn random(&mut self, dst: &mut [u8]) -> Result {
        dst.fill(17u8.wrapping_add(self.random_calls));
        self.random_calls = self.random_calls.wrapping_add(1);
        Ok(())
    }
    fn unix_millis(&mut self) -> Result<i64> {
        Ok(1_790_765_296_000)
    }
}
