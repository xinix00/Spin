//! De VFS leent tijdens elke C-callback kort de exclusieve opslag-eigenaar.
use crate::{Error, FileId, OpenFlags, Result, Storage, ffi::Callbacks};
use core::{
    ffi::{CStr, c_char, c_int, c_void},
    slice,
};

pub(crate) fn callbacks<B: Storage>(backend: &mut B) -> Callbacks {
    Callbacks {
        context: core::ptr::from_mut(backend).cast(),
        open: open::<B>,
        close: close::<B>,
        read: read::<B>,
        write: write::<B>,
        truncate: truncate::<B>,
        sync: sync::<B>,
        size: size::<B>,
        remove: remove::<B>,
        exists: exists::<B>,
        random: random::<B>,
        time: time::<B>,
        cooperate: cooperate::<B>,
    }
}
fn code(result: Result) -> c_int {
    match result {
        Ok(()) => 0,
        Err(e) => e.code,
    }
}
/// Leent de eigenaar uitsluitend gedurende één niet-herintredende callback.
///
/// # Safety
/// `p` komt uit `callbacks::<B>` en de exclusieve boot-runtime leeft nog.
unsafe fn owner<'a, B: Storage>(p: *mut c_void) -> &'a mut B {
    // SAFETY: De runtime houdt B op hetzelfde adres en serialiseert alle calls.
    unsafe { &mut *p.cast::<B>() }
}
unsafe extern "C" fn open<B: Storage>(
    p: *mut c_void,
    name: *const c_char,
    flags: c_int,
    out: *mut u32,
) -> c_int {
    // SAFETY: De C-VFS levert een afgesloten naam, geldig out en dezelfde eigenaar.
    unsafe {
        match owner::<B>(p).open(CStr::from_ptr(name), OpenFlags(flags)) {
            Ok(id) => {
                *out = id.0;
                0
            }
            Err(e) => e.code,
        }
    }
}
unsafe extern "C" fn close<B: Storage>(p: *mut c_void, id: u32) -> c_int {
    // SAFETY: Het handvat komt uit open en deze callback is exclusief.
    code(unsafe { owner::<B>(p) }.close(FileId(id)))
}
unsafe extern "C" fn read<B: Storage>(
    p: *mut c_void,
    id: u32,
    off: u64,
    dst: *mut u8,
    n: c_int,
) -> c_int {
    let Ok(n) = usize::try_from(n) else {
        return Error::IO.code;
    };
    if n == 0 {
        return 0;
    }
    // SAFETY: SQLite bezit de schrijfbare buffer van n bytes gedurende xRead.
    let dst = unsafe { slice::from_raw_parts_mut(dst, n) };
    dst.fill(0);
    // SAFETY: De runtime houdt de exclusieve eigenaar in leven.
    match unsafe { owner::<B>(p) }.read(FileId(id), off, dst) {
        Ok(got) if got == n => 0,
        Ok(got) if got < n => {
            // xRead vereist nul na EOF, ook als een backend voorbij got schreef.
            if let Some(tail) = dst.get_mut(got..) {
                tail.fill(0);
            }
            522
        }
        Ok(_) => Error::IO.code,
        Err(e) => e.code,
    }
}
unsafe extern "C" fn write<B: Storage>(
    p: *mut c_void,
    id: u32,
    off: u64,
    src: *const u8,
    n: c_int,
) -> c_int {
    let Ok(n) = usize::try_from(n) else {
        return Error::IO.code;
    };
    if n == 0 {
        return 0;
    }
    // SAFETY: SQLite leent precies n leesbare bytes en herintrede is uitgesloten.
    unsafe { code(owner::<B>(p).write(FileId(id), off, slice::from_raw_parts(src, n))) }
}
unsafe extern "C" fn truncate<B: Storage>(p: *mut c_void, id: u32, n: u64) -> c_int {
    // SAFETY: De runtime houdt de exclusieve eigenaar in leven.
    code(unsafe { owner::<B>(p) }.truncate(FileId(id), n))
}
unsafe extern "C" fn sync<B: Storage>(p: *mut c_void, id: u32, flags: c_int) -> c_int {
    // SAFETY: De runtime houdt de exclusieve eigenaar in leven.
    code(unsafe { owner::<B>(p) }.sync(FileId(id), flags))
}
unsafe extern "C" fn size<B: Storage>(p: *mut c_void, id: u32, out: *mut u64) -> c_int {
    // SAFETY: De C-VFS levert een geldig out en dezelfde exclusieve eigenaar.
    unsafe {
        match owner::<B>(p).size(FileId(id)) {
            Ok(n) => {
                *out = n;
                0
            }
            Err(e) => e.code,
        }
    }
}
unsafe extern "C" fn remove<B: Storage>(p: *mut c_void, name: *const c_char, sync: c_int) -> c_int {
    // SAFETY: De C-VFS leent een afgesloten naam gedurende deze callback.
    unsafe { code(owner::<B>(p).remove(CStr::from_ptr(name), sync != 0)) }
}
unsafe extern "C" fn exists<B: Storage>(
    p: *mut c_void,
    name: *const c_char,
    out: *mut c_int,
) -> c_int {
    // SAFETY: De C-VFS levert een afgesloten naam, geldig out en dezelfde eigenaar.
    unsafe {
        match owner::<B>(p).exists(CStr::from_ptr(name)) {
            Ok(found) => {
                *out = i32::from(found);
                0
            }
            Err(e) => e.code,
        }
    }
}
unsafe extern "C" fn random<B: Storage>(p: *mut c_void, dst: *mut u8, n: c_int) -> c_int {
    let Ok(len) = usize::try_from(n) else {
        return 0;
    };
    if len == 0 {
        return 0;
    }
    // SAFETY: SQLite leent n schrijfbare bytes en vraagt één exclusieve callback.
    let dst = unsafe { slice::from_raw_parts_mut(dst, len) };
    dst.fill(0);
    // SAFETY: De runtime houdt de eigenaar in leven.
    if unsafe { owner::<B>(p) }.random(dst).is_ok() {
        n
    } else {
        dst.fill(0);
        0
    }
}
unsafe extern "C" fn time<B: Storage>(p: *mut c_void, out: *mut i64) -> c_int {
    // SAFETY: De C-VFS levert een geldig out en dezelfde exclusieve eigenaar.
    unsafe {
        match owner::<B>(p).unix_millis() {
            Ok(n) => {
                *out = n;
                0
            }
            Err(e) => e.code,
        }
    }
}

unsafe extern "C" fn cooperate<B: Storage>(p: *mut c_void) -> c_int {
    // SAFETY: De progress-handler loopt binnen de unieke SQLite-eigenaar;
    // herintrede is uitgesloten, zoals bij de VFS-callbacks.
    code(unsafe { owner::<B>(p) }.cooperate())
}
