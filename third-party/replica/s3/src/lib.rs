//! Replica over leans3/SigV4, met dezelfde parkeerbare eigenaar als SQLite.
#![no_std]
#![forbid(unsafe_code)]
extern crate alloc;
use alloc::vec::Vec;
use core::{
    pin::Pin,
    task::{Context, Poll},
};
use leans3::{AsyncWrite, Client, IoError, Transport};
use replica_core::object::{Object, Store, StoreError};
use replica_sqlite::asynchronous::Suspend;
/// Eén client, transport en wachtbrug; annulering maakt deze eigenaar onbruikbaar.
pub struct S3<T, W> {
    client: Client,
    transport: T,
    wait: W,
    poisoned: bool,
}
impl<T: Transport, W: Suspend> S3<T, W> {
    /// Leent geen globale sockets; transport verzorgt TLS, DNS en totale deadlines.
    /// Bijvoorbeeld Hop's `SlotTransport`, of de hostadapter voor integratietoetsen.
    pub fn new(client: Client, transport: T, wait: W) -> Result<Self, StoreError> {
        let out = Self {
            client,
            transport,
            wait,
            poisoned: false,
        };
        out.ready()?;
        Ok(out)
    }
    fn ready(&self) -> Result<(), StoreError> {
        if self.poisoned {
            return Err(StoreError::Cancelled);
        }
        if self.client.now.is_none_or(|now| now() < 1_577_836_800)
            || self.client.endpoint.is_empty()
            || self.client.bucket.is_empty()
            || self.client.region.is_empty()
            || self.client.access_key_id.is_empty()
            || self.client.secret_access_key.is_empty()
        {
            return Err(StoreError::Configuration);
        }
        Ok(())
    }
}
fn map(e: leans3::Error) -> StoreError {
    match e {
        leans3::Error::NotFound => StoreError::Missing,
        leans3::Error::Status(s) if matches!(s.code, 401 | 403) => StoreError::Denied,
        leans3::Error::ObjectTooLarge { .. }
        | leans3::Error::ListPageTooLarge
        | leans3::Error::OutOfMemory => StoreError::Limit,
        leans3::Error::EndpointRequired
        | leans3::Error::BucketRequired
        | leans3::Error::KeyRequired
        | leans3::Error::EndpointIncomplete
        | leans3::Error::EndpointScheme
        | leans3::Error::ClockRequired
        | leans3::Error::ListMaxZero => StoreError::Configuration,
        _ => StoreError::Transport,
    }
}
impl<T: Transport, W: Suspend> Store for S3<T, W> {
    fn directories(
        &mut self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<alloc::string::String>, StoreError> {
        self.ready()?;
        if limit == 0 || limit > replica_core::manifest::MAX_COMMITS {
            return Err(StoreError::Limit);
        }
        match self.wait.wait(
            self.client
                .list_directories(&mut self.transport, prefix, limit),
        ) {
            Ok(Ok((keys, false))) => {
                if keys.iter().any(|key| {
                    key.len() <= prefix.len()
                        || key.len() > 1024
                        || !key.starts_with(prefix)
                        || !key.ends_with('/')
                        || key[prefix.len()..key.len() - 1].contains('/')
                }) {
                    return Err(StoreError::Transport);
                }
                Ok(keys)
            }
            Ok(Ok((_, true))) => Err(StoreError::Limit),
            Ok(Err(e)) => Err(map(e)),
            Err(_) => {
                self.poisoned = true;
                Err(StoreError::Cancelled)
            }
        }
    }
    fn list_batch(&mut self, prefix: &str, limit: usize) -> Result<Vec<Object>, StoreError> {
        self.ready()?;
        if limit == 0 || limit > 1000 {
            return Err(StoreError::Limit);
        }
        let keys = match self
            .wait
            .wait(self.client.list(&mut self.transport, prefix, limit))
        {
            Ok(result) => result.map_err(map)?.0,
            Err(_) => {
                self.poisoned = true;
                return Err(StoreError::Cancelled);
            }
        };
        let mut out = Vec::new();
        out.try_reserve_exact(keys.len())
            .map_err(|_| StoreError::Limit)?;
        for key in keys {
            if key.len() > 1024 || !key.starts_with(prefix) {
                return Err(StoreError::Transport);
            }
            out.push(Object { key, size: None });
        }
        Ok(out)
    }
    fn put(&mut self, key: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.ready()?;
        if bytes.len() > replica_core::segment::MAX_BYTES {
            return Err(StoreError::Limit);
        }
        let options = leans3::PutOptions::default();
        match self
            .wait
            .wait(self.client.put(&mut self.transport, key, bytes, &options))
        {
            Ok(result) => result.map(|_| ()).map_err(map),
            Err(_) => {
                self.poisoned = true;
                Err(StoreError::Cancelled)
            }
        }
    }
    fn get(&mut self, key: &str, limit: usize) -> Result<Vec<u8>, StoreError> {
        self.ready()?;
        if limit > replica_core::segment::MAX_BYTES {
            return Err(StoreError::Limit);
        }
        let mut sink = Sink {
            bytes: Vec::new(),
            limit,
            limited: false,
        };
        let result = self
            .wait
            .wait(self.client.get_to(&mut self.transport, key, &mut sink));
        match result {
            Err(_) => {
                self.poisoned = true;
                Err(StoreError::Cancelled)
            }
            Ok(result) => {
                if sink.limited {
                    return Err(StoreError::Limit);
                }
                result.map_err(map)?;
                Ok(sink.bytes)
            }
        }
    }
    fn delete(&mut self, key: &str) -> Result<(), StoreError> {
        self.ready()?;
        let options = leans3::DeleteOptions::default();
        match self
            .wait
            .wait(self.client.delete(&mut self.transport, key, &options))
        {
            Ok(Ok(())) | Ok(Err(leans3::Error::NotFound)) => Ok(()),
            Ok(Err(e)) => Err(map(e)),
            Err(_) => {
                self.poisoned = true;
                Err(StoreError::Cancelled)
            }
        }
    }
    fn list(&mut self, prefix: &str, limit: usize) -> Result<Vec<Object>, StoreError> {
        self.ready()?;
        if limit == 0 || limit > replica_core::manifest::MAX_COMMITS {
            return Err(StoreError::Limit);
        }
        let (keys, truncated) =
            match self
                .wait
                .wait(self.client.list(&mut self.transport, prefix, limit))
            {
                Ok(result) => result.map_err(map)?,
                Err(_) => {
                    self.poisoned = true;
                    return Err(StoreError::Cancelled);
                }
            };
        if truncated {
            return Err(StoreError::Limit);
        }
        let mut out = Vec::new();
        out.try_reserve_exact(keys.len())
            .map_err(|_| StoreError::Limit)?;
        for key in keys {
            if key.len() > 1024 || !key.starts_with(prefix) {
                return Err(StoreError::Transport);
            }
            out.push(Object { key, size: None });
        }
        Ok(out)
    }
}
struct Sink {
    bytes: Vec<u8>,
    limit: usize,
    limited: bool,
}
impl AsyncWrite for Sink {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<Result<usize, IoError>> {
        let this = self.get_mut();
        let Some(end) = this
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|end| *end <= this.limit)
        else {
            this.limited = true;
            return Poll::Ready(Err(IoError::Other("replica object budget")));
        };
        if end > this.bytes.capacity() {
            let target = end.next_power_of_two().min(this.limit);
            if this
                .bytes
                .try_reserve_exact(target - this.bytes.len())
                .is_err()
            {
                this.limited = true;
                return Poll::Ready(Err(IoError::Other("replica allocation")));
            }
        }
        this.bytes.extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }
}
