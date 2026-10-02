//! SQLite-bestanden in één exclusief gemounte map; iedere sync gaat naar HopFS.
use applib::{appnet::SystemClient, stacktask::Suspender, sys};
use core::{ffi::CStr, future::Future};
use replica_sqlite::{
    Error, FileId, OpenFlags, Result,
    asynchronous::{AsyncStorage, Cancelled, Suspend},
};
/// De applicatie levert klok en entropie; deze adapter verzint geen tijd of randombron.
pub trait Environment {
    /// Vul alle bytes met de platformbron, of faal.
    fn random(&mut self, dst: &mut [u8]) -> Result;
    /// Unixmilliseconden; ongeldige/niet gesynchroniseerde klok mag falen.
    fn unix_millis(&mut self) -> Result<i64>;
}
/// De kleine adapter van de runtime naar SQLite's parkeercontract.
pub struct Wait<'a>(pub &'a Suspender);
impl Suspend for Wait<'_> {
    fn wait<F: Future>(&self, f: F) -> core::result::Result<F::Output, Cancelled> {
        self.0.wait(f).map_err(|_| Cancelled)
    }
}
#[derive(Clone, Copy)]
struct File {
    name: [u8; 256],
    len: usize,
    generation: u32,
    open: bool,
    writable: bool,
}
impl File {
    const EMPTY: Self = Self {
        name: [0; 256],
        len: 0,
        generation: 0,
        open: false,
        writable: false,
    };
}
struct Path {
    bytes: [u8; 512],
    len: usize,
}
impl Path {
    fn text(&self) -> Result<&str> {
        core::str::from_utf8(&self.bytes[..self.len]).map_err(|_| Error::TEXT)
    }
}
/// Eén eigenaar van SystemClient en maximaal zestien SQLite-handvatten.
/// Geen andere schrijver mag deze root gebruiken; de jobspec gebruikt recreate.
pub struct Files<'a, E> {
    sys: SystemClient,
    root: &'a str,
    env: E,
    files: [File; 16],
    poisoned: bool,
}
impl<'a, E: Environment> Files<'a, E> {
    /// Root is een canonieke absolute map op het persistente volume.
    pub fn new(sys: SystemClient, root: &'a str, env: E) -> Result<Self> {
        if !root.starts_with('/')
            || root.len() > 255
            || root == "/"
            || root.ends_with('/')
            || !canonical(&root.as_bytes()[1..])
        {
            return Err(Error::CANNOT_OPEN);
        }
        Ok(Self {
            sys,
            root,
            env,
            files: [File::EMPTY; 16],
            poisoned: false,
        })
    }
    fn path(&self, name: &[u8]) -> Result<Path> {
        if name.is_empty() || name.len() > 255 || !canonical(name) {
            return Err(Error::CANNOT_OPEN);
        }
        let mut path = Path {
            bytes: [0; 512],
            len: self.root.len() + 1 + name.len(),
        };
        path.bytes[..self.root.len()].copy_from_slice(self.root.as_bytes());
        path.bytes[self.root.len()] = b'/';
        path.bytes[self.root.len() + 1..path.len].copy_from_slice(name);
        Ok(path)
    }
    fn file(&self, id: FileId) -> Result<&File> {
        if self.poisoned {
            return Err(Error::IO);
        }
        self.files
            .get((id.0 & 255) as usize)
            .filter(|f| f.open && f.generation == id.0 >> 8)
            .ok_or(Error::IO)
    }
    fn file_path(&self, id: FileId, writable: bool) -> Result<Path> {
        let f = self.file(id)?;
        if writable && !f.writable {
            return Err(Error { code: 8 });
        }
        self.path(&f.name[..f.len])
    }
    fn error(&mut self, e: sys::Error) -> Error {
        if matches!(
            e,
            sys::Error::Timeout | sys::Error::Transport(_) | sys::Error::Protocol(_)
        ) {
            self.poisoned = true;
        }
        Error::IO
    }
}
fn canonical(name: &[u8]) -> bool {
    core::str::from_utf8(name).is_ok()
        && !name.iter().any(|&b| b < 32 || b == 127 || b == b'\\')
        && name
            .split(|&b| b == b'/')
            .all(|s| !s.is_empty() && s != b"." && s != b"..")
}
impl<E: Environment> AsyncStorage for Files<'_, E> {
    async fn cooperate(&mut self) -> Result {
        if self.poisoned {
            return Err(Error::IO);
        }
        let mut pending = true;
        core::future::poll_fn(|cx| {
            if pending {
                pending = false;
                cx.waker().wake_by_ref();
                core::task::Poll::Pending
            } else {
                core::task::Poll::Ready(())
            }
        })
        .await;
        Ok(())
    }
    fn invalidate(&mut self) {
        self.poisoned = true;
    }
    async fn open(&mut self, name: &CStr, flags: OpenFlags) -> Result<FileId> {
        if self.poisoned {
            return Err(Error::IO);
        }
        let bytes = name.to_bytes();
        let path = self.path(bytes)?;
        if self
            .files
            .iter()
            .any(|f| f.open && &f.name[..f.len] == bytes)
        {
            return Err(Error { code: 5 });
        }
        let index = self
            .files
            .iter()
            .position(|f| !f.open && f.generation < 0xffffff)
            .ok_or(Error::FULL)?;
        match self.sys.stat(path.text()?).await {
            Ok(_) => {}
            Err(sys::Error::NotFound { .. }) if flags.is_create() && flags.is_writable() => {
                self.sys
                    .truncate(path.text()?, 0)
                    .await
                    .map_err(|e| self.error(e))?;
            }
            Err(sys::Error::NotFound { .. }) => return Err(Error::CANNOT_OPEN),
            Err(e) => return Err(self.error(e)),
        }
        let f = &mut self.files[index];
        f.generation += 1;
        f.name[..bytes.len()].copy_from_slice(bytes);
        f.len = bytes.len();
        f.open = true;
        f.writable = flags.is_writable();
        Ok(FileId((f.generation << 8) | index as u32))
    }
    async fn close(&mut self, id: FileId) -> Result {
        self.file(id)?;
        self.files[(id.0 & 255) as usize].open = false;
        Ok(())
    }
    async fn read(&mut self, id: FileId, offset: u64, dst: &mut [u8]) -> Result<usize> {
        let path = self.file_path(id, false)?;
        let mut done = 0;
        while done < dst.len() {
            let end = dst.len().min(done + sys::MAX_CHUNK);
            let off = offset.checked_add(done as u64).ok_or(Error::RANGE)?;
            let n = self
                .sys
                .read_into(path.text()?, off, &mut dst[done..end])
                .await
                .map_err(|e| self.error(e))?;
            if n == 0 {
                break;
            }
            if n > end - done {
                self.poisoned = true;
                return Err(Error::IO);
            }
            done += n;
        }
        Ok(done)
    }
    async fn write(&mut self, id: FileId, offset: u64, src: &[u8]) -> Result {
        let path = self.file_path(id, true)?;
        let mut done = 0;
        for chunk in src.chunks(sys::MAX_CHUNK) {
            let off = offset.checked_add(done as u64).ok_or(Error::RANGE)?;
            let n = self
                .sys
                .write_at(path.text()?, off, chunk)
                .await
                .map_err(|e| self.error(e))?;
            if n != chunk.len() {
                self.poisoned = true;
                return Err(Error::IO);
            }
            done += n;
        }
        Ok(())
    }
    async fn truncate(&mut self, id: FileId, size: u64) -> Result {
        let p = self.file_path(id, true)?;
        self.sys
            .truncate(p.text()?, size)
            .await
            .map_err(|e| self.error(e))
    }
    async fn sync(&mut self, id: FileId, _flags: i32) -> Result {
        let p = self.file_path(id, false)?;
        self.sys.sync(p.text()?).await.map_err(|e| self.error(e))?;
        Ok(())
    }
    async fn size(&mut self, id: FileId) -> Result<u64> {
        let p = self.file_path(id, false)?;
        self.sys.stat(p.text()?).await.map_err(|e| self.error(e))
    }
    async fn remove(&mut self, name: &CStr, sync_directory: bool) -> Result {
        if self.poisoned {
            return Err(Error::IO);
        }
        let bytes = name.to_bytes();
        let mut p = self.path(bytes)?;
        if self
            .files
            .iter()
            .any(|f| f.open && &f.name[..f.len] == bytes)
        {
            return Err(Error { code: 5 });
        }
        match self.sys.remove(p.text()?).await {
            Ok(()) | Err(sys::Error::NotFound { .. }) => {}
            Err(e) => return Err(self.error(e)),
        }
        if sync_directory {
            p.len = p.bytes[..p.len]
                .iter()
                .rposition(|&b| b == b'/')
                .ok_or(Error::CANNOT_OPEN)?;
            self.sys.sync(p.text()?).await.map_err(|e| self.error(e))?;
        }
        Ok(())
    }
    async fn exists(&mut self, name: &CStr) -> Result<bool> {
        if self.poisoned {
            return Err(Error::IO);
        }
        let p = self.path(name.to_bytes())?;
        match self.sys.stat(p.text()?).await {
            Ok(_) => Ok(true),
            Err(sys::Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(self.error(e)),
        }
    }
    fn random(&mut self, dst: &mut [u8]) -> Result {
        if self.poisoned {
            return Err(Error::IO);
        }
        self.env.random(dst)
    }
    fn unix_millis(&mut self) -> Result<i64> {
        if self.poisoned {
            return Err(Error::IO);
        }
        self.env.unix_millis()
    }
}
