# Replication review, 25 September 2026

The review started from v1.29.34, including the source-guard race reproduced
against v1.29.33. These are implementation tests on SQLite and local storage;
they do not certify HopOS's volume durability or the deployment topology.

## Reproduced and corrected

| Failure | Correction and regression coverage |
| --- | --- |
| A tracked commit finishes while the source guard waits; the old empty dirty count produces a false foreign-write alarm | Read tracker state inside the read transaction. `guard_race_test.go` covers the actual connection-pool wait, 4/64 KiB pages, commit, rollback, cancellation, page-size changes and real untracked writes |
| A live capture grows or shrinks between segments, then compaction rejects its differing segment sizes | Replay each segment's truncation while selecting compacted pages. `TestLiveCaptureSizeChangesSurviveCompaction` restores before and after compaction for growth, shrink and shrink/grow |
| The marker remembers the minimum size instead of the final committed size, making later growth checks report a false gap | Keep final size and minimum truncation size separately. `TestLiveCaptureRecordsFinalSize` |
| Catch-up allocates all changed page bytes at once, ignoring SegmentBytes | Spool catch-up in bounded segments inside the final read transaction. `TestLiveCaptureCatchupSegmentsStayBounded` |
| A busy writer prevents an empty catch-up round forever | Finish at the reconciliation transaction and keep later writes pending. `TestLiveCaptureFinishesUnderContinuousWrites` checks both the captured point and the next sync |
| VACUUM changes page size between capture transactions; old page offsets can be published | Check page size in every capture transaction, abort and retry as a new snapshot. `TestLiveCaptureRejectsPageSizeChangeMidCopy` verifies the old point remains usable |
| Only the first child window contains writes, so higher-tier compaction never selects it | Assign a child by its ending boundary. `TestCompactionIncludesFirstChildWindow` checks roll-up, expiry and restore |
| The bucket accepts current but its reply is lost; local fallback keeps writing the previous generation | Read back current, complete a confirmed publication, and retain the recovery marker when read-back also fails. The crash matrix now checks the bucket pointer, not only explicitly selected generations; `TestRenewalWithUnknownCurrentOutcome` covers retry/restart with both publication outcomes |
| A completed renewal retains Previous; after a foreign write and failed repair it can fall back to an ancestor whose changes are no longer tracked | Clear Previous on completion and invalidation. `TestInvalidatedGenerationCannotFallBackToItsAncestor` includes a marker left by an older build |
| A lease read error is treated as a free database, or the holder continues after renewal failure/expiry | Fail closed on acquisition read errors and damaged records; notify the holder and stop renewing on uncertain ownership. `lease_test.go` injects read/write failure, expiry, missing files and malformed records |
| A restart replays an older dirty log into an unclean tracker but leaves its marker clean; later writes skip marker invalidation | Persist an unclean marker whenever pages are replayed. `TestDirtyLogReplayCannotHideLaterWrites` injects a failed log rewrite, restarts, writes again, damages the log, and checks that recovery preserves the new write |

`TestLiveCatchupFailureRetriesAllPages` injects read, spool-write, spool-sync
and cancellation failures after catch-up starts, both for an increment and a
renewal. It verifies that the previous point remains usable and retry includes
all pages. `TestLiveCaptureModel` uses four deterministic seeds with repeated
writes, growth, shrink and VACUUM between capture transactions. Each committed
point is restored and checked with SQLite's integrity check and row comparison.
The existing crash matrix and randomized `TestModel` additionally cover
restarts, local failures, bucket failures, retention and advertised points.
The model's dirty-log fault injection now reaches the log's own Storage adapter;
previously replacing only Replica.files did not inject that fault. Its restart
checks permit conservative replay only of the pages actually named by the
persisted log, and permit a fresh generation after a failed log rewrite only
when that log cannot continue the generation.

## Litestream comparison

The upstream [database tests](https://github.com/benbjohnson/litestream/blob/main/db_test.go)
exercise snapshot contents, exclusion of later writes, multi-level compaction
and retention. Those invariants are relevant here. Their WAL/LTX fixtures
cannot be dropped into this VFS/segment implementation; the new tests exercise
the corresponding invariants through this library's actual SQLite adapter,
capture, object store, compaction and restore. No upstream test code was copied.

## Validation completed

All of these passed on the final implementation:

```sh
go test ./...
REPLICA_MODEL_SEED=20260925 REPLICA_MODEL_STEPS=600 go test -race ./replica ./internal/persistence ./internal/tenancy -count=1
REPLICA_MODEL_SEED=20260926 REPLICA_MODEL_STEPS=600 go test ./replica -run '^TestModel$' -count=1 -v
```

The second model seed committed 130 states, fetched and checked 398 restore
points, and traversed 38 generations. The dedicated replay regression was
observed failing before the marker/tracker fix and passing afterwards.

## Production requirements and remaining limits

- **One writer is a host requirement.** The file lease is not atomic. Two
  processes can both read a missing/expired lease before either writes it, and
  an old process paused beyond its TTL is not fenced at each database write.
  Enforce one instance per shared database and per bucket namespace, serialize
  starts and use recreate updates. A multi-writer-safe design needs an atomic
  host lock plus fencing; a longer settle sleep does not provide that.
- **Do not infer durability from these tests.** The tests exercise ordinary
  filesystem SQLite and injected storage faults. HopOS must preserve the
  documented write/sync ordering across actual power loss. A volume-level
  crash/reboot test and a restore of a realistically sized database remain
  deployment validation work.
- **Copies still delay incremental uploads.** A generation copy finishes and
  uploads before the next increment. The final reconciliation can pause local
  writers in proportion to the dirty set; segment-sized page buffers bound
  memory, not that pause. Page-number metadata still scales with database size.
- **Corrupt leases fail closed.** Investigate the storage problem and establish
  that there is no live writer before removing a damaged lease to restart.
- **Existing bad backups are not retroactively certified.** These changes
  prevent the reproduced publication errors and can compact valid older
  variable-size captures. They cannot recover bytes that an older malformed
  capture never shipped. Use an actual restore and SQLite integrity check to
  assess an existing generation.
