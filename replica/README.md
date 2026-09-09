# SQLite page replication

`replica` is a schema-independent Go library for SQLite replication through the
[ncruces/go-sqlite3](https://github.com/ncruces/go-sqlite3) VFS. It has no imports
from Spin. Its tests use a small SQLite table, an in-memory object store, a fake
clock and fault-injecting storage adapters. Spin's environment parsing lives in
`internal/replicaconfig`; Spin supplies its database adapter and lifecycle.

## Embedding

1. Construct `New` or `NewWithOptions` with a namespace and the underlying VFS.
2. Call `Prepare(ctx)` **before** opening SQLite.
3. Open SQLite using `VFSName()`, rollback-journal mode (`journal_mode=DELETE`),
   and a single connection. All database writes must go through this VFS.
4. `Attach` a `Database` whose `WithReadTransaction` excludes every writer until
   its callback returns. Execute a real table read inside that transaction to
   acquire the database lock; `SELECT 1` does not acquire it.
5. Call `Sync(ctx)` yourself or use `Start(ctx)`. Cancel the host context, close
   the database and call `Close()` before reopening the namespace. `Close`
   cancels and drains active syncs before unregistering the VFS.

See `example_test.go` for the SQL adapter. `Options.Objects`, `Options.Storage`
and `Options.Now` replace the external dependencies. `OSStorage()` and
`VFSStorage(vfs)` are the standard local adapters. The storage adapter must
access the same bytes as SQLite's VFS. The default object adapter is `S3`, whose
HTTP client can also be replaced.

Requirements: exactly one writer per namespace; atomic object PUT and strongly
consistent GET/LIST; durable file Sync and durable creation/deletion in local
Storage. WAL, direct writes that bypass the tracking VFS, and concurrent writers
in another process are unsupported. A lockless VFS is usable only when the host
serializes all writers through the supplied Database adapter.

## Commit and recovery protocol

- A sync takes the entire dirty set **inside one read transaction**, including
  database size, and spools its segments locally. SQLite's writer is released
  before any network transfer. A generation starts with all pages; a page-size
  change starts a new generation. The host runs that first, whole copy before
  it serves (`SnapshotDue`).
- A **page index** next to the database (`<db>.replica-index`) holds a hash per
  page of what the bucket has, stamped with the marker sequence it describes
  and written after every committed sync; a failed write removes it. A start
  after an unclean stop hashes the database, compares, and continues the
  generation with the pages that differ. A missing, torn, foreign or stale
  index (sequence behind the marker) costs a full snapshot, never a missed
  page: a stale index would hide a page that returned to its old bytes.
- A window is `(start, end]`: a commit exactly on a boundary belongs to the
  window that ends there, the rule `plan` uses for a point at that boundary, so
  a point restores the same database before and after compaction. A commit
  never lands at or before the sealed frontier.
- Pruning an expired generation removes visibility first (its snapshot
  manifest, then other manifests, then data): an interrupted prune leaves only
  orphaned data, never an advertised point that cannot be fetched.
- Parts use fresh random object names. Only a final manifest PUT publishes a
  batch. An upload failure exposes either the previous complete state or the
  new complete state, never a subset. An uncertain manifest acknowledgment
  abandons the local generation so its sequence cannot be reused with new data.
- The tracker revision identifies writes since capture. It holds its lock while
  persisting a clean marker. A subsequent write durably invalidates the marker
  before touching the database; invalidation failure rejects the SQLite write.
- The snapshot has its own permanent manifest and parts. Compaction merges only
  incremental batches, using their contiguous sequence ranges and checksums.
  Each manifest also records the minimum database size reached, so merging a
  shrink followed by growth cannot resurrect pages from the original snapshot.
  A window becomes visible only after its completion manifest. Expiration
  removes a finer manifest before its parts, and only under complete coverage.
- Restore begins at the snapshot and follows contiguous committed sequence
  ranges. `Fetch` selects the latest retained point at or before its timestamp;
  a zero timestamp selects the latest state. It rejects missing sequence ranges
  and missing/corrupt referenced parts. `Points` reports retained points with
  nanosecond timestamp precision; round-trip the timestamp without truncation.
- Downloads are validated in a scratch database before touching the destination.
  Since a generic VFS has no rename operation, publication writes a durable
  `.replica-restoring` intent, copies and syncs the destination, then removes the
  intent durably. `Prepare` retries interrupted publication even if the database
  already exists. `Fetch` must target an offline file and refuses the live DB.
- In-process restore readers are protected from concurrent compaction/pruning.
  A process restart with an unclean marker starts a fresh snapshot. An idle
  restored generation retains its sequence number and compaction frontier.

Layout (manifest format version 2):

```text
<prefix>/<namespace>/current
<prefix>/<namespace>/generations/<generation>/snapshot
<prefix>/<namespace>/generations/<generation>/L0/<sequence>-<nanoseconds>.json
<prefix>/<namespace>/generations/<generation>/L<level>/<start>-<end>/complete
<prefix>/<namespace>/generations/<generation>/data/<attempt>/<part>.seg
```

Memory holds at most a few segment buffers plus dirty-page and manifest
metadata. Local scratch space must hold the captured dirty data (a full database
for a snapshot); restore needs one full scratch database. Sync uses one reusable
spool name, so crashes cannot accumulate one local spool per attempt. Failed
remote attempts may leave unreferenced parts; generation pruning removes these
along with the rest of the generation. They never influence restore planning.

## Spin integration

Spin uses its existing `SPIN_S3_PREFIX` setting (default `spin`) without adding a
version namespace. Both server entrypoints instantiate this library through the
tenant lifecycle; health status and the restore-point API use the same instance.
A backup imported through Spin's normal restore interface writes through the
tracking VFS and is replicated on the next sync.

Local clean markers are bound to their destination: changing the endpoint,
bucket or prefix forces a new snapshot. The library rejects legacy manifests
with `ErrLegacyFormat` because they cannot prove complete batches or windows.

## Tests and extraction

```sh
go test ./replica ./internal/replicaconfig
go test -race ./replica ./internal/persistence ./internal/tenancy ./internal/server
```

The failure suite covers incomplete and uncertain uploads, partial window
retries, interrupted expiration, writes during upload, marker and spool I/O
failures, restore download/checksum/publication failures, clean/unclean restart,
page-size changes, clock rollback, permanent snapshots and all advertised tiered
points. Restored SQLite fixtures undergo `PRAGMA integrity_check` and content
comparison. S3 tests independently check canonical signing, escaping, pagination
and HTTP error handling. The segment decoder also has a fuzz target.

To extract this directory into its own repository, copy it with a `go.mod`
requiring `github.com/ncruces/go-sqlite3 v0.35.4` and Go 1.26.4, run `go mod tidy`,
and update the self-import in `example_test.go` to the chosen module path. No
Spin schema, server, environment parser, or HopOS dependency needs to move.
The current wire magic `SPINSEG1` is retained as a format identifier.
