//! De heap op de host: over een gewone buffer, met na elke stap de
//! invarianten van `State` via [`Heap::check`].

use super::{Core, GlobalAlloc, HDR, Layout, MAX_ALIGN, MIN_BLOCK, Walk};
type Heap = super::Heap<TestCore>;
struct TestCore;
impl Core for TestCore {
    fn id() -> u64 {
        std::thread_local! { static ID: u64 = {
            static NEXT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
            NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        }; }
        ID.with(|id| *id)
    }
}
use std::vec::Vec;

/// Een heap over een eigen, op een pagina uitgelijnde buffer.
struct Arena {
    _buf: Vec<u8>,
    heap: Heap,
    base: usize,
    size: usize,
}

impl Arena {
    fn new(size: usize) -> Self {
        let mut buf = vec![0u8; size + MAX_ALIGN];
        let raw = buf.as_mut_ptr().expose_provenance();
        let base = (raw + MAX_ALIGN - 1) & !(MAX_ALIGN - 1);
        let heap = Heap::new();
        // SAFETY: `[base, base + size)` ligt in `buf`, dat de arena bezit en
        // dat de test alleen via de heap aanraakt.
        unsafe { heap.init(base, base + size) };
        Self {
            _buf: buf,
            heap,
            base,
            size,
        }
    }

    fn alloc(&self, size: usize, align: usize) -> Option<usize> {
        self.heap.reserve(size, align)
    }

    fn free(&self, p: usize) {
        // SAFETY: de tests geven alleen terug wat ze kregen, of toetsen met
        // opzet een dubbele of vreemde vrijgave binnen de eigen buffer.
        unsafe { self.heap.release(p) }
    }

    fn walk(&self) -> Walk {
        self.heap.check().unwrap()
    }

    /// Alles vrij: één blok over het hele gebied.
    fn assert_whole(&self) {
        let w = self.walk();
        assert_eq!(w.blocks, 1, "{w:?}");
        assert_eq!(w.free_bytes, self.size);
        assert_eq!(self.heap.used(), 0);
    }
}

/// Vult het lijf `[p, p + n)` met `tag`.
fn fill(p: usize, n: usize, tag: u8) {
    // SAFETY: `[p, p + n)` is een lijf dat de heap aan de test gaf en dat
    // nog niet terug is.
    unsafe { core::ptr::write_bytes(core::ptr::with_exposed_provenance_mut::<u8>(p), tag, n) }
}

/// Zijn alle bytes van `[p, p + n)` nog `tag`?
fn intact(p: usize, n: usize, tag: u8) -> bool {
    // SAFETY: als bij `fill`.
    let s = unsafe { core::slice::from_raw_parts(core::ptr::with_exposed_provenance::<u8>(p), n) };
    s.iter().all(|&b| b == tag)
}

#[test]
fn an_empty_heap_refuses_and_counts() {
    let h = Heap::new();
    assert_eq!(h.reserve(8, 8), None);
    assert_eq!(h.stats().failed, 1);
    assert_eq!(h.check(), Ok(Walk::default()));
    // SAFETY: een gebied op 0 of kleiner dan één blok wordt nooit
    // aangeraakt; de heap blijft leeg.
    unsafe {
        h.init(0, 0x1000);
        assert_eq!(h.capacity(), 0);
        h.init(0x1000, 0x1010);
        assert_eq!(h.capacity(), 0);
    }
    assert_eq!(h.reserve(1, 1), None);
}

#[test]
fn blocks_are_aligned_up_to_a_page() {
    let a = Arena::new(64 << 10);
    let p = a.alloc(1, 1).unwrap();
    assert_eq!(p, a.base + HDR);
    assert_eq!(a.heap.used(), MIN_BLOCK as u64);
    for align in [2, 8, 16, 32, 64, 128, 512, 1024, 4096] {
        let q = a.alloc(24, align).unwrap();
        assert_eq!(q % align, 0, "align {align}");
        a.walk();
    }
    // Boven de pagina, of geen macht van twee: geweigerd en geteld.
    assert_eq!(a.alloc(8, 8192), None);
    assert_eq!(a.alloc(8, 48), None);
    assert_eq!(a.alloc(8, 0), None);
    assert_eq!(a.heap.stats().failed, 3);
    a.walk();
}

#[test]
fn every_free_order_coalesces_back_to_one_block() {
    let sizes = [1, 16, 17, 100, 4096, 3, 999, 64, 12_000, 7];
    let orders: [fn(usize) -> Vec<usize>; 3] = [
        |n| (0..n).rev().collect(),                               // LIFO
        |n| (0..n).collect(),                                     // FIFO
        |n| (0..n).step_by(2).chain((1..n).step_by(2)).collect(), // om en om
    ];
    for order in orders {
        let a = Arena::new(256 << 10);
        let ps: Vec<usize> = (0..40)
            .map(|i| a.alloc(sizes[i % sizes.len()], 1 << (i % 7)).unwrap())
            .collect();
        for i in order(ps.len()) {
            a.free(ps[i]);
            a.walk();
        }
        a.assert_whole();
        let s = a.heap.stats();
        assert_eq!((s.allocs, s.frees, s.bad_frees), (40, 40, 0));
    }
}

#[test]
fn holes_are_reused_before_the_tail() {
    let a = Arena::new(128 << 10);
    let ps: Vec<usize> = (0..64).map(|_| a.alloc(240, 16).unwrap()).collect();
    for p in ps.iter().step_by(2) {
        a.free(*p);
    }
    let w = a.walk();
    assert_eq!(w.free_blocks, 32 + 1, "32 gaten en de staart"); // 256-byte gaten
    let holes: Vec<usize> = ps.iter().step_by(2).copied().collect();
    // Past precies in een gat: komt uit een gat (het laatst vrijgegeven,
    // de lijst per klasse is LIFO), niet van de staart.
    let q = a.alloc(240, 16).unwrap();
    assert!(holes.contains(&q));
    assert_eq!(q, ps[62]);
    // Past in geen gat: van de staart, en de gaten blijven.
    let big = a.alloc(4096, 16).unwrap();
    assert!(big > *ps.last().unwrap());
    assert_eq!(a.walk().free_blocks, 31 + 1);
    // Een bezet blok tussen twee gaten vrij: drie worden er één.
    a.free(ps[1]);
    assert_eq!(a.walk().free_blocks, 30 + 1);
    a.free(q);
    assert_eq!(a.walk().free_blocks, 31 + 1);
    for p in ps.iter().skip(3).step_by(2) {
        a.free(*p);
    }
    a.free(big);
    a.assert_whole();
}

#[test]
fn a_double_or_foreign_free_is_ignored_and_counted() {
    let a = Arena::new(16 << 10);
    let p = a.alloc(64, 8).unwrap();
    let q = a.alloc(64, 8).unwrap();
    a.free(p);
    a.free(p);
    a.free(a.base + 8);
    a.free(0);
    a.free(usize::MAX);
    assert_eq!(a.heap.stats().bad_frees, 4);
    a.walk();
    a.free(q);
    a.assert_whole();
}

#[test]
fn global_alloc_hands_out_and_takes_back() {
    let a = Arena::new(16 << 10);
    let l = Layout::from_size_align(100, 64).unwrap();
    // SAFETY: een geldige layout; het blok gaat terug met dezelfde.
    unsafe {
        let p = GlobalAlloc::alloc(&a.heap, l);
        assert!(!p.is_null());
        assert_eq!(p.addr() % 64, 0);
        GlobalAlloc::dealloc(&a.heap, p, l);
        let huge = Layout::from_size_align(1 << 20, 8).unwrap();
        assert!(GlobalAlloc::alloc(&a.heap, huge).is_null());
    }
    a.assert_whole();
}

/// Een xorshift64: deterministisch per zaad, zonder crate.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> usize {
        (self.next() % n) as usize
    }
}

/// Een willekeurig leven van allocaties en vrijgaven: elk lijf wordt
/// gevuld en bij vrijgave nagekeken (een overlap of een kop in een lijf
/// laat een byte omvallen), en om de zoveel stappen de hele heap.
fn stress(seed: u64, ops: usize) {
    const CAP: usize = 1 << 20;
    let a = Arena::new(CAP);
    let mut rng = Rng(seed);
    let mut live: Vec<(usize, usize, u8)> = Vec::new();
    let mut failed = 0u64;
    for op in 0..ops {
        if live.is_empty() || rng.below(100) < 55 {
            let size = match rng.below(100) {
                0..70 => 1 + rng.below(256),
                70..95 => 257 + rng.below(8192 - 256),
                _ => 8193 + rng.below(65536 - 8192),
            };
            let align = [1, 2, 8, 16, 16, 16, 64, 256, 4096][rng.below(9)];
            match a.alloc(size, align) {
                Some(p) => {
                    assert_eq!(p % align, 0);
                    assert!(p >= a.base + HDR && p + size <= a.base + CAP);
                    let tag = (op % 251) as u8;
                    fill(p, size, tag);
                    live.push((p, size, tag));
                }
                None => failed += 1,
            }
        } else {
            let (p, size, tag) = live.swap_remove(rng.below(live.len() as u64));
            assert!(
                intact(p, size, tag),
                "seed {seed} op {op}: payload overwritten"
            );
            a.free(p);
        }
        if op % 97 == 0 {
            let w = a.walk();
            let s = a.heap.stats();
            assert_eq!(w.free_bytes as u64 + s.used, CAP as u64);
        }
    }
    let s = a.heap.stats();
    assert_eq!(s.failed, failed);
    assert!(s.peak <= CAP as u64);
    for (p, size, tag) in live.drain(..) {
        assert!(intact(p, size, tag));
        a.free(p);
    }
    a.assert_whole();
    assert_eq!(a.heap.stats().bad_frees, 0);
}

#[test]
fn seeded_random_stress_keeps_the_invariants() {
    for seed in [1, 42, 0xdead_beef, 0x9e37_79b9_7f4a_7c15] {
        stress(seed, 20_000);
    }
}

#[test]
fn concurrent_allocations_and_cross_thread_frees_keep_ownership() {
    let arena = Arena::new(4 << 20);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for tag in 1..=4u8 {
            let a = &arena;
            handles.push(scope.spawn(move || {
                let mut keep = Vec::new();
                for i in 0..2000 {
                    let p = a.alloc(1024, 64).unwrap();
                    fill(p, 1024, tag);
                    if i % 100 == 0 {
                        keep.push((p, tag));
                    } else {
                        assert!(intact(p, 1024, tag));
                        a.free(p);
                    }
                }
                keep
            }));
        }
        for h in handles {
            for (p, tag) in h.join().unwrap() {
                assert!(intact(p, 1024, tag));
                arena.free(p);
            }
        }
    });
    arena.assert_whole();
    assert_eq!(arena.heap.stats().bad_frees, 0);
}
#[test]
fn reserved_core_identity_cannot_acquire_the_lock() {
    struct Reserved;
    impl Core for Reserved {
        fn id() -> u64 {
            u64::MAX
        }
    }
    let h = super::Heap::<Reserved>::new();
    assert!(h.reserve(16, 16).is_none());
}
/// De werklast van Stulp (JSON-waarden, strings, staatskopieën): veel kleine
/// blokken van wisselende maat, door elkaar terug en opnieuw. Geeft de
/// gemiddelde tijd per alloc+free en het aantal lijststappen; draai met
/// `cargo test -p heap --release -- --ignored json_like --nocapture`.
#[test]
#[ignore = "meting, geen toets"]
fn json_like_churn_cost() {
    let a = Arena::new(32 << 20);
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let size = |r: u64| -> usize {
        match r % 100 {
            0..=69 => 8 + (r as usize >> 8) % 56, // korte strings, kleine objecten
            70..=94 => 64 + (r as usize >> 8) % 192, // grotere strings, vecs
            _ => 256 + (r as usize >> 8) % 3840,  // buffers
        }
    };
    let mut live: Vec<usize> = Vec::new();
    for _ in 0..40_000 {
        let r = next();
        live.push(a.alloc(size(r), 8).unwrap());
    }
    let ops: u32 = 400_000;
    let t0 = std::time::Instant::now();
    for _ in 0..ops {
        let r = next();
        let i = (r as usize >> 3) % live.len();
        a.free(live[i]);
        live[i] = a.alloc(size(next()), 8).unwrap();
    }
    let dt = t0.elapsed();
    let w = a.walk();
    std::println!(
        "json_like: {:.0} ns per free+alloc over {ops} ops; free blocks {}",
        dt.as_nanos() as f64 / f64::from(ops),
        w.free_blocks
    );
}

/// De vorm van Stulp (03-10): duizenden vrije blokken van 32 bytes (vraag
/// 16) en dan vragen van 48 (vraag 32), tot de exacte klassen allebei in de
/// klasse 32 tot 63. Draai met
/// `cargo test -p heap --release -- --ignored stulp_shape --nocapture`.
#[test]
#[ignore = "meting, geen toets"]
fn stulp_shape_cost() {
    let a = Arena::new(32 << 20);
    let mut small = Vec::new();
    let mut keep = Vec::new();
    for _ in 0..4547 {
        small.push(a.alloc(16, 8).unwrap());
        // Een bezet blok ertussen, anders smelten de vrije samen.
        keep.push(a.alloc(16, 8).unwrap());
    }
    for b in small {
        a.free(b);
    }
    let ops: u32 = 100_000;
    let mut p = a.alloc(32, 8).unwrap();
    let t0 = std::time::Instant::now();
    for _ in 0..ops {
        a.free(p);
        // Een vraag van 16 neemt het blok van 48 dat net vrijkwam (de rest
        // is kleiner dan een blok), zodat de vraag van 32 geen passend blok
        // vooraan vindt en de 32'ers langs moet, of eroverheen springt.
        keep.push(a.alloc(16, 8).unwrap());
        p = a.alloc(32, 8).unwrap();
    }
    let dt = t0.elapsed();
    let w = a.walk();
    std::println!(
        "stulp_shape: {:.0} ns per free+alloc+alloc met {} vrije blokken",
        dt.as_nanos() as f64 / f64::from(ops),
        w.free_blocks
    );
    a.free(p);
    for b in keep {
        a.free(b);
    }
}
