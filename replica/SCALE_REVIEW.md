# Max review and scale tests, 25 September 2026

This review starts from v1.29.35 (`bc852208afe2`). An independent
GPT-6 Astra reviewer with maximum reasoning reproduced failures against that
baseline, then reviewed the fixes. This document describes the subsequent
working-tree changes, not the contents of v1.29.35.

## Correctness fixes

| Failure | Fix and regression coverage |
| --- | --- |
| A rewritten dirty-log header reaches disk, but its write or Sync reports failure. Later writes go to the old log; restart selects the newer file and can miss those writes. | Switch the active file before attempting its header. After an uncertain header outcome, later database syncs persist an invalidation record in that same file. Failures before the header attempt retain the previous log. `TestDirtyLogUncertainRewriteCannotHideLaterWrites` exercises six late-failure variants with real writes, restart and restore. |
| Deleting an expired generation's snapshot manifest fails, but pruning continues and deletes its data. The still-advertised point becomes unrestorable. | Stop deleting that generation after any failure. `TestPrunePreservesDataWhenSnapshotDeleteFails` checks the old point remains advertised and fetchable. |
| An idle restart with a clock behind the last sync lowers the compaction frontier. Later writes can change a previously advertised restore point. | Keep the durable sealed frontier monotonic. `TestIdleRestartCannotLowerCompactionFrontier` restores the previous point after restart and another write. |
| A slow marker Sync crosses a compaction boundary. Compaction reads the clock again and seals a window past the durable frontier. | Pass the exact persisted frontier into compaction. `TestCompactionCannotSealPastDurableFrontier` advances the clock inside marker Sync, restarts with a clock rollback and checks the original point. |
| A renewal interrupted after clock rollback sorts before its predecessor and cannot recover through the existing lexical generation comparison. | Recognize the explicit Previous relationship first. `TestInterruptedRenewalDoesNotDependOnGenerationOrder` restarts with that marker. |
| Ordinary growth through 1 GiB leaves SQLite's reserved lock-byte page unwritten; coverage checks mistake it for lost tracking and request a fresh snapshot. | Exclude exactly that reserved page. Coverage tests use 512-, 4096- and 65536-byte pages and still reject adjacent real holes. The real scale test grows through the boundary, stays in the same generation and restores every row. |
| Lease loss during a long Prepare/restore cannot find the not-yet-published tenant; the opening can later publish a database without its lease. | Give each opening its own cancellation context and identity, synchronize publication with lease loss, and finish cleanup before allowing retry. Close cancels and waits for openings outside the tenant mutex. Three tenancy tests cover loss during Prepare, loss just before publication, stale callbacks and shutdown during opening. |

SQLite documents the special page in its
[file-format specification](https://www.sqlite.org/fileformat.html#the_lock_byte_page).
This exception does not waive coverage checks on ordinary database pages.
The tenancy Prepare regression was also run against baseline code using a Go
overlay and failed there before passing with the fix.

## Reductions in allocation and work

- Full snapshots generate a segment's page numbers at a time. They no longer
  insert every database page into a map and sort a second database-wide slice.
  Only actual tracked writes are retained for retry. Retrying the snapshot
  itself still copies the full range; an existing generation can continue
  shipping changes while a failed renewal waits to retry.
- Taking dirty pages allocates the exact output length and releases the old map.
  Unused tracking fields, duplicate accessors and a forwarding capture function
  were removed.
- Dirty-log record I/O uses fixed buffers of at most 64 KiB, avoiding a second
  encoded copy of every page number. Replay still returns the required uint32
  page-number slice. Chunk-boundary, short-read, short-write and torn-append tests
  cover the streaming implementation.
- Restore prefetch uses a ring of slots bounded by its concurrency, instead of
  allocating a channel for every part. A test wraps the ring repeatedly while
  deliberately delaying the first part and checks both ordering and backpressure.
- Compaction loads one metadata layout and incorporates its newly committed
  windows across tiers. It no longer reloads every manifest for every tier, or
  lists segment objects to discover that layout. Redundant copies of manifest
  fields were removed. The metadata-layout regression verifies one LIST of the
  generation's `L` prefix and one snapshot GET during two-tier compaction.
- Growth coverage indexes only the newly added tail. A three-page append to a
  virtual 1 TiB database no longer builds a bitmap for the preceding terabyte.

The persisted segment, manifest and dirty-log formats are unchanged. The
follow-up recovery work adds an optional `repair_from` field to the local JSON
marker; the format version remains unchanged.
Checksums, coverage checks, source guarding, publication ordering, dirty tracking
and recovery fallbacks remain necessary and were retained.

## Automatic recovery and safe refusal

A follow-up review tested the requirement that detected damage should initiate
recovery. Previously, compaction only invalidated a generation on a sequence gap;
missing parts, checksum failures and corrupt manifests could fail repeatedly
without ever starting a replacement. A missing snapshot manifest did not even
make compaction fail. The four regression cases were observed failing before
the correction.

Read errors now distinguish proven archive damage from transport/permission
failures. Sync schedules a new snapshot for damaged committed objects, including
damage found while resolving an uncertain publication. Temporary GET, LIST, PUT
and DELETE failures preserve the generation and retry. Missing snapshots still
mean an unfinished/pruned generation to historical discovery, but are an error
when compacting the active committed generation.

The replacement is always made from the local source; it never overwrites that
source from a damaged archive. `repair_from` records the proven damaged lineage
durably, survives successive failed replacement copies and clock rollback, and
is cleared after successful publication. It is not a fallback generation. If an
uncertain publication moved `current` to a replacement which then became
damaged, the next repair records that replacement as its source lineage. A
different bucket lineage is refused even when its timestamp sorts earlier.

Malformed older manifests no longer block discovery of a repaired current
generation: they are omitted with a warning. Current-generation metadata errors
and transient listing/read failures remain errors. Parts are validated when
read, so a historical point listed from valid metadata is not a complete
integrity certification of all its parts.

The bootstrap check also refuses to create an empty database when both the
local database and bucket `current` are absent but generation objects remain.
Failure to list the archive also refuses bootstrap. It does not guess which
generation should be current. The tests repair that pointer explicitly and
then verify the original data restores.

Regression coverage in `selfheal_test.go`, `repair_lineage_test.go` and
`bootstrap_safety_test.go` includes missing parts, checksum errors, malformed
metadata, missing snapshots, a failed repair upload followed by restart, clock
rollback, uncertain publications, healthy and damaged foreign generations,
normal background-loop recovery, transient errors and missing-pointer bootstrap.
Successful repairs restore the expected contents and pass SQLite's integrity
check. These tests verify preservation of local source data as well as recovery.

This does not promise universal detection or repair:

- Ordinary sync does not reread existing snapshot parts or continuously scrub
  the entire bucket. Damage there is found when those parts are read, such as
  during restore. Nor does each sync revalidate the `current` pointer.
- Capture does not run a full SQLite integrity check on the source. Replicating
  source bytes cannot repair source corruption. Spin verifies composed restore
  scratch databases before publishing them.
- Network/storage outages must end before retries can succeed. A torn local
  marker or damaged lease can require intervention because ownership can no
  longer be established safely. Those records are not silently discarded.
- A new snapshot provides a new point from the present source; it cannot
  reconstruct historical bytes already lost from both source and archive.

The final policy is automatic retry or replacement when safe, otherwise an
explicit error with existing data retained. The single-writer and storage
durability requirements below still apply.

Follow-up validation passed: the full application suite; a further 600-step
model run with race detection across replica, persistence and tenancy; targeted
race runs for the final lineage, background-recovery and bootstrap cases; and
all seven platform/component builds, including both HopOS architectures. The
additional broad commands were:

```sh
go test ./... -timeout=15m
REPLICA_MODEL_SEED=20260929 REPLICA_MODEL_STEPS=600 go test -race ./replica ./internal/persistence ./internal/tenancy -count=1 -timeout=30m
```

The follow-up build used `PUBLISH=0` and wrote only local artifacts under
`dist/v1.29.36-selfheal-review`. These recovery changes have not been published.

## KISS consolidation

The final simplification pass removes duplication without introducing another
recovery framework:

- Source guarding, missing growth pages and damaged archive objects now report
  their causes to `Sync`. That single boundary schedules repair and records the
  failure; individual detectors no longer mutate recovery state or log their
  own repair policy. The existing repair-publication preflight still records
  proven lineage before starting a replacement.
- Snapshot status and execution use one decision for age, uploaded bytes and
  renewal backoff. The duplicate `compactionDue` policy is removed.
- Adoption returns the dirty-log pages it already validated. Prepare and runtime
  continuation use them directly, instead of reading the same log repeatedly.
- `dirtyLog.ready` is removed: an active file already represents that state.
- Dirty-log rewriting no longer sorts a set of page numbers. Capture still
  sorts them where ordered reads matter.
- A second compaction-window filter is removed; the manifest and range checks
  already prove its condition.

Previous, RepairFrom and Uncertain retain distinct meanings: a resumable
fallback, non-resumable provenance, and an unresolved publication. Collapsing
them would discard information needed to fail safely after a crash. No new
configuration, worker, repair loop or persisted field was added by this pass.

Validation after consolidation passed:

```sh
go test ./... -timeout=15m
REPLICA_MODEL_SEED=20260930 REPLICA_MODEL_STEPS=600 go test -race ./replica ./internal/persistence ./internal/tenancy -count=1 -timeout=30m
```

The replica race run completed in 122 seconds. Existing targeted crash,
dirty-log, live-capture, compaction and repair tests also passed. All seven
platform/component builds passed with `PUBLISH=0`, including HopOS arm64 and
riscv64 (`dist/v1.29.36-kiss-review`). No release was published.

## Validation

These commands passed:

```sh
REPLICA_MODEL_SEED=20260928 REPLICA_MODEL_STEPS=1200 go test ./... -timeout=30m
REPLICA_MODEL_SEED=20260927 REPLICA_MODEL_STEPS=1200 go test -race ./replica ./internal/persistence -count=1 -timeout=30m
go test -race ./internal/tenancy -count=1 -timeout=3m
go test ./replica -run '^$' -fuzz '^FuzzDecodeSegment$' -fuzztime=30s -parallel=2
REPLICA_SCALE_MIB=1024 go test ./replica -run '^TestReplicaScale$' -count=1 -v -timeout=30m
go test ./replica -run '^$' -bench 'Benchmark(GrowthGap|DirtyLog)' -benchmem -benchtime=3x
```

The full application suite passed. The replica/persistence race run completed
in about 160 seconds, with another independent 1,200-step model seed in the full
suite. The final tenancy race run passed after its shutdown changes. Decoder
fuzzing executed 786,442 inputs without failure in 30 seconds. These are bounded
test runs, not an exhaustive proof of all failure interleavings.

The local release compile gate also passed for Linux amd64/arm64 server and
client, macOS arm64 client, and HopOS arm64/riscv64 server. It used the pinned
HopOS metal v2.2.7 dependency and the installed Tamago toolchain. `PUBLISH=0`
kept these working-tree artifacts local (`dist/v1.29.36-review`); no tag or
release was published by this review.

The real scale test uses SQLite and an object store on local disk, with 4 MiB
segments and a small SQLite cache. It starts with a 975 MiB database, grows it
past 1 GiB to about 1,040 MiB, then updates 25% and 50% of rows with overlap.
It checks four complete restores: initial snapshot, growth, overlapping updates,
and two-tier compaction after finer manifests expire. Every restore passes
`PRAGMA integrity_check` and compares every row's ID, revision and payload with
the independent expected contents. The bucket and expected database contents
are not held in memory.

| Phase | Sampled peak Go heap | Uploaded data |
| --- | ---: | ---: |
| Initial snapshot | 20.57 MiB | 976.08 MiB |
| Growth increment | 12.97 MiB | 65.09 MiB |
| First update increment | 22.35 MiB | 272.29 MiB |
| Second update increment | 24.00 MiB | 528.56 MiB |
| Two-tier compaction | 29.95 MiB | 1,121.29 MiB |
| Restore after compaction | 41.86 MiB | — |

The complete run took 55.8 seconds on the development machine. Total bucket
traffic was 8,249.25 MiB read and 2,963.31 MiB written, including all restores and
both compaction tiers. Heap values sample `runtime.MemStats.HeapAlloc` every
10 ms, with a GC before each phase; they are not RSS, hard upper bounds or a
projection for larger databases. Timing overlapped other test work and local
disk storage avoids production network latency.

`TestTerabyteSnapshotStartsWithOneSegmentAndRetriesOnlyActualWrites` uses a
virtual 1 TiB file and cancels after the first segment. It proves snapshot
startup and failure retry do not enumerate or retain every page. It does not
copy, upload or restore a real terabyte. The tail-coverage benchmark used 10 B/op
in the short run. Replaying a one-million-page dirty log used 4,071,960 B/op in
9 allocations, chiefly the required four-byte page numbers plus its I/O buffer.

## Limits before deployment on very large databases

- Actual production database sizes, available RAM and write rates were not
  supplied. This review validates about 1 GiB end to end and specific virtual
  1 TiB allocation paths. It does not validate real 100 GiB or 1 TiB throughput,
  restore time, peak RAM or HopOS scheduling.
- Dirty tracking, the dirty log's deduplication map, capture/replay page lists
  and compaction's winning-page map still scale with the number of changed
  pages. A large rewrite can require substantial RAM even with small segments.
  Restore also has a database-wide coverage bitmap: about 32 MiB per TiB at
  4 KiB pages, before slice capacity and other metadata. The existing dirty-log
  reader rejects files larger than 1 GiB, requiring a fresh snapshot after an
  unclean restart. That represents roughly 512 GiB of distinct 4 KiB page
  records, with memory likely constraining such a write set earlier.
- Compaction still reads input parts twice per tier to find and copy surviving
  pages. Avoiding that needs a different index or external merge design. Bucket
  I/O, number of parts/manifests and compaction lag need measurement on the
  expected workload. Generation discovery and pruning still list the full
  namespace; narrowing the compaction-layout listing does not change those.
- A full snapshot needs local scratch approximately the database size, plus
  pages rewritten during its copy and encoding overhead. Restore needs a full
  scratch database as well as its destination. Copies delay incremental uploads;
  final reconciliation holds a read transaction for the dirty set and can delay
  writers. Bounded page buffers do not bound that pause.
- The file lease remains advisory, not an atomic multi-process lock or write
  fence. Enforce one writer per database and bucket namespace and serialize
  starts, including recreate deployments. The startup cancellation fix does not
  make simultaneous writers safe.
- Both HopOS architectures need runtime validation on the real storage stack.
  A realistic-sized backup/restore rehearsal and power-loss/reboot exercise
  remain deployment checks. Compilation and injected desktop filesystem errors
  cannot establish HopOS's actual durable Sync ordering.

The scale test is intentionally opt-in above its small default. Set
`REPLICA_SCALE_MIB` to the intended payload and provide roughly five times that
much free disk; the fixture builds on disk and removes each verified restore
before the next. Run it with a suitable timeout and measure actual process/RSS,
disk, bucket throughput and application write latency on the target platform.
