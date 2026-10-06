//! `SPIN_BENCH`: de meetlat naast Spin, als eigen Hop-job op dezelfde node.
//!
//! Dezelfde opslaglaag (`Backend` over HopFS), dezelfde segmentcodering en
//! hash als een capture, hetzelfde S3-pad als de uploader; alleen de cijfers,
//! daarna stopt de app. Zo staat naast elke trage capture wat de node kan.
use super::*;
use alloc::{string::String, vec::Vec};
use core::{future::poll_fn, pin::Pin};
use replica_core::{
    local::{File, Name},
    object::Store,
    segment,
};
use replica_sqlite::{OpenFlags, Storage};
fn now_ms() -> u64 {
    applib::clock::now_ns() / 1_000_000
}
fn rate(bytes: u64, ms: u64) -> u64 {
    bytes / ms.max(1) / 1000
}
fn storage_failure(error: replica_core::Error) -> Error {
    applib::log!("SPIN_BENCH_STORAGE_FAILED error={error:?}");
    Error::Http(500, "bench storage failure")
}
/// Pseudowillekeurige vulling: niet comprimeerbaar, wel reproduceerbaar.
fn fill(block: &mut [u8], seed: u64) {
    let mut x = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    for chunk in block.chunks_mut(8) {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let bytes = x.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}
pub(super) async fn run(app: &'static App, net: &'static appnet::Net, root: &'static str) -> Result {
    let mb: u64 = app
        .env("SPIN_BENCH_MB")
        .unwrap_or("1024")
        .parse()
        .map_err(failure)?;
    let files = storage::FilesPool::new(
        Files::new(
            net.system_client(),
            root,
            Environment {
                app,
                random: Random::open(app)?,
            },
        )
        .map_err(failure)?,
    );
    let client = if app.env("SPIN_S3_ENDPOINT").is_some_and(|s| !s.trim().is_empty()) {
        Some(storage::s3_client(app).map_err(failure)?)
    } else {
        None
    };
    let prefix = spin_domain::try_string(app.env("SPIN_S3_PREFIX").unwrap_or("spin-bench"))?;
    let files = &files;
    // SAFETY: One bounded stack owns the only storage handle and S3 connection of
    // this app; nothing else runs beside it.
    let mut task = unsafe {
        Task::new(8 << 20, move |s| -> Result {
            let wait = Wait(s);
            let mut b = storage::Backend::new(
                files,
                &wait,
                storage::Location::Root(String::from("bench.sqlite")),
            );
            disk(&mut b, mb).map_err(storage_failure)?;
            if let Some(client) = client {
                let mut remote = storage::Bucket::new(client, crate::s3::network, Wait(s))
                    .map_err(|_| Error::Http(503, "invalid S3 configuration"))?;
                s3(&mut remote, &prefix).map_err(storage_failure)?;
            }
            applib::log!("SPIN_BENCH_DONE");
            Ok(())
        })
    }
    .map_err(|_| Error::Http(503, "bench stack allocation failed"))?;
    poll_fn(|cx| Pin::new(&mut task).poll(cx)).await
}
/// Schrijven, ruw lezen per chunkgrootte, en de capture-stappen per segmentgrootte.
fn disk(b: &mut storage::Backend<'_>, mb: u64) -> replica_core::Result {
    const BLOCK: usize = 1 << 20;
    const PAGE: u32 = 4096;
    let source = Name::new("spin.sqlite")?;
    let spool = Name::new("spin.sqlite.replica-capture")?;
    let size = mb << 20;
    let mut block = Vec::new();
    replica_core_reserve(&mut block, BLOCK)?;
    block.resize(BLOCK, 0);
    // A. Schrijven in blokken van 1 MiB, dan sync.
    let t0 = now_ms();
    let mut file = File::open(b, &source, true)?;
    file.truncate(0)?;
    for i in 0..mb {
        fill(&mut block, i);
        file.write(i << 20, &block)?;
    }
    file.sync()?;
    file.close()?;
    let ms = now_ms().saturating_sub(t0);
    applib::log!("SPIN_BENCH_WRITE mb={mb} ms={ms} mb_s={}", rate(size, ms));
    // B. Ruw lezen in één open bestand, per chunkgrootte (de capture leest via
    // File::read in 64 KiB met een cooperate per stuk).
    for chunk in [64usize << 10, 1 << 20, 4 << 20] {
        let mut buffer = Vec::new();
        replica_core_reserve(&mut buffer, chunk)?;
        buffer.resize(chunk, 0);
        let t0 = now_ms();
        let id = b.open(source.cstr()?, OpenFlags(2))?;
        let mut offset = 0;
        while offset < size {
            let n = b.read(id, offset, &mut buffer)?;
            if n == 0 {
                break;
            }
            offset += n as u64;
        }
        b.close(id)?;
        let ms = now_ms().saturating_sub(t0);
        applib::log!(
            "SPIN_BENCH_READ chunk_kb={} ms={ms} mb_s={}",
            chunk >> 10,
            rate(size, ms)
        );
    }
    let t0 = now_ms();
    let mut file = File::open(b, &source, false)?;
    let mut offset = 0;
    while offset < size {
        file.read(offset, &mut block)?;
        offset += BLOCK as u64;
    }
    file.close()?;
    let ms = now_ms().saturating_sub(t0);
    applib::log!("SPIN_BENCH_READ chunk_kb=64 via=File ms={ms} mb_s={}", rate(size, ms));
    // C. De capture-stappen: per segment open/lees/sluit, coderen, hashen, spool.
    for seg in [1usize << 20, 4 << 20, 16 << 20] {
        let mut data = Vec::new();
        replica_core_reserve(&mut data, seg)?;
        data.resize(seg, 0);
        let (mut read_ms, mut encode_ms, mut hash_ms, mut write_ms) = (0, 0, 0, 0);
        let mut out = 0u64;
        let mut spool_file = File::open(b, &spool, true)?;
        spool_file.truncate(0)?;
        spool_file.close()?;
        let per_segment = seg / PAGE as usize;
        let mut offset = 0u64;
        let started = now_ms();
        while offset < size {
            let len = (size - offset).min(seg as u64) as usize;
            let t0 = now_ms();
            let mut file = File::open(b, &source, false)?;
            file.read(offset, &mut data[..len])?;
            file.close()?;
            let t1 = now_ms();
            let first = (offset / u64::from(PAGE)) as u32 + 1;
            let mut records = Vec::new();
            replica_core_reserve(&mut records, per_segment)?;
            for (i, bytes) in data[..len].chunks_exact(PAGE as usize).enumerate() {
                records.push((first + i as u32, bytes));
            }
            let encoded = segment::encode(PAGE, size, &records)?;
            let t2 = now_ms();
            let digest = replica_core::hash(&encoded);
            let t3 = now_ms();
            let mut file = File::open(b, &spool, false)?;
            file.write(out, &encoded)?;
            file.close()?;
            let t4 = now_ms();
            out += encoded.len() as u64;
            read_ms += t1 - t0;
            encode_ms += t2 - t1;
            hash_ms += t3 - t2;
            write_ms += t4 - t3;
            offset += len as u64;
            core::hint::black_box(digest);
        }
        let mut file = File::open(b, &spool, false)?;
        file.sync()?;
        file.close()?;
        let total = now_ms().saturating_sub(started);
        applib::log!(
            "SPIN_BENCH_CAPTURE seg_mb={} read_mb_s={} encode_mb_s={} hash_mb_s={} write_mb_s={} total_ms={total} total_mb_s={}",
            seg >> 20,
            rate(size, read_ms),
            rate(size, encode_ms),
            rate(size, hash_ms),
            rate(out, write_ms),
            rate(size, total)
        );
    }
    let _ = b.remove(spool.cstr()?, false);
    let _ = b.remove(source.cstr()?, false);
    Ok(())
}
/// PUT en GET van delen zoals de uploader ze stuurt, in één verbinding na elkaar.
fn s3(remote: &mut storage::Bucket<'_>, prefix: &str) -> replica_core::Result {
    for (part_mb, count) in [(4usize, 8u32), (16, 2)] {
        let mut block = Vec::new();
        replica_core_reserve(&mut block, part_mb << 20)?;
        block.resize(part_mb << 20, 0);
        let mut keys = Vec::new();
        replica_core_reserve(&mut keys, count as usize)?;
        for i in 0..count {
            keys.push(
                spin_domain::try_string(&alloc::format!("{prefix}/bench/{part_mb}m-{i:02}.seg"))
                    .map_err(|_| replica_core::Error::Memory)?,
            );
        }
        let t0 = now_ms();
        for (i, key) in keys.iter().enumerate() {
            fill(&mut block, i as u64);
            remote.put(key, &block)?;
        }
        let ms = now_ms().saturating_sub(t0);
        let bytes = (part_mb << 20) as u64 * u64::from(count);
        applib::log!(
            "SPIN_BENCH_S3_PUT part_mb={part_mb} parts={count} ms={ms} mb_s={}",
            rate(bytes, ms)
        );
        let t0 = now_ms();
        for key in &keys {
            let got = remote.get(key, part_mb << 20)?;
            core::hint::black_box(got.len());
        }
        let ms = now_ms().saturating_sub(t0);
        applib::log!(
            "SPIN_BENCH_S3_GET part_mb={part_mb} parts={count} ms={ms} mb_s={}",
            rate(bytes, ms)
        );
        for key in &keys {
            remote.delete(key)?;
        }
    }
    Ok(())
}
fn replica_core_reserve<T>(v: &mut Vec<T>, n: usize) -> replica_core::Result {
    v.try_reserve_exact(n)
        .map_err(|_| replica_core::Error::Memory)
}
