# Design: what this replica is, and why it holds

`README.md` says how to embed this package. This file says what it guarantees,
which failures it is built against, and where each guard sits. It exists
because on 22 September 2026 a live tenant lost a day of data with every single
object in the bucket present and correct, and the lesson was not "add a check"
but "name the invariant, then have something enforce it".

## The one guarantee

> A restore point produces the database exactly as it was at that moment, or it
> fails saying why. It never produces a file that merely looks like one.

Everything below is in service of that sentence. Note what it does not promise:
it does not promise that every moment is a restore point (the schedule thins
them out), and it does not promise a restore point exists at all when the
source was never fully shipped. It promises that what it hands back is whole,
or that nobody gets a broken file believing otherwise.

## The model in one screen

A **generation** is one lineage: a full snapshot plus every change after it. A
**commit** is one atomic step in that lineage, described by a **manifest** with
a contiguous sequence range (`FirstSeq..Seq`), the database size at that point
(`MinSize`) and the **parts** that carry the pages. A part is a **segment**:
`SPINSEG1`, page size, database size, page count, then raw SQLite pages, then a
sha256 of all of it. The `current` object names the generation a restore starts
from. Manifests are rolled up into **windows** (L1 quarter hours, L2 hours, L3
days) so old points thin out without losing the ability to restore them.

Why pages and not SQLite's WAL, the way Litestream does it: a slot on HopOS
runs one writer, in one process, through one VFS, in rollback-journal mode.
Watching writes at the VFS is simpler than parsing a WAL, and it works on a
storage layer that has no filesystem. The price is that the design depends on
seeing every write, and that price is what the guards below pay.

## What can go wrong, and what stops it

| Failure | What stops it |
| --- | --- |
| A part is corrupted or truncated in the bucket | Every part carries its size and sha256; `readPart` verifies both before a byte is used |
| A manifest is forged, malformed, or from an older format | `readManifest` validates version, sequence order, window bounds and part keys |
| A crash between two syncs | The dirty log next to the database names the pages of writes that were in flight; it is synced before the pages it protects, so a page can only be on disk when the log names it |
| A crash mid-sync | Parts get fresh random names and only the final manifest PUT publishes them; an interrupted sync leaves orphans, never a visible half commit |
| A crash mid-restore | The restore composes in a scratch file and publishes under a durable intent, retried at the next start |
| An interrupted prune | Visibility is removed before data, so what remains is orphaned data, never an advertised point that cannot be fetched |
| The page size changes | A new generation with a full snapshot; old page numbers mean nothing |
| A gap in the commit sequence | Compaction refuses to merge across it, and the generation ends: the next sync starts a fresh one |
| **Two processes on one database** (a rolling update on a shared volume) | **The host must serialize starts and use recreate updates; `lease.go` detects existing holders but is not an atomic lock** |
| **A commit PUT whose outcome is unknown** (a 503, a lost reply) | **`Uncertain` in the marker: the next sync looks in the bucket, counts the commit or retries its sequence** |
| **A stop between a commit and its marker write** | **`adoptLocal` continues at the bucket's tip with the pages the dirty log names since the marker** |
| **A renewal that fails, or a stop in the middle of one** | **`Previous` in the marker and `resetToLog`: the renewed generation goes on, the renewal waits ten minutes** |
| **The database grows by pages this replica never saw** | **The commit gate: `growthGap`, `coverage.go`** |
| **A write that went around the tracking VFS** | **The source guard: `checkSource`, `guard.go`** |
| **A database that lost writes winning over the replica** | **`adoptLocal`, `guard.go`** |
| **A composition that is short of the size it records** | **`pageSet.shortfall`, `coverage.go`** |
| **A composed file SQLite would reject** | **`Options.Verify`, the host's `PRAGMA quick_check`** |

The first five in bold are new since 22 September, the last four since 23
September. The rows above them were there all along and held; they are why the
bucket was intact while the data was still unreachable.

## Crashes: the database here is the truth

On 23 September a crash was followed by a Spin that copied its whole database
again, after every restart, and ran out of memory doing so. Three causes, and
one rule that answers all three.

The rule, as Litestream has it: **the database file is the truth for
everything it wrote.** A crash never makes the bucket more right than the
file. Litestream's `checkDatabaseBehindReplica` moves its own position to the
replica's and never writes the replica over the database; neither does this
package, except for a file that has nothing unshipped and whose bucket moved to
a generation it never made (a stale copy).

- **One writer.** HopOS starts the new slot of a rolling update while the old
  one runs, on the same volume: two processes wrote one SQLite file, one dirty
  log and one generation. That is where the gaps in the commit sequence and the
  markers behind the bucket came from. The lease (`lease.go`, the equivalent of
  Litestream's `lock.json`) sits next to the database because both slots share
  the volume; a holder renews it every five seconds, a start waits until it is
  released or has not been renewed for thirty. The Spin job also runs with
  `update_policy: recreate`. The file lease is advisory: without an atomic
  create/compare-and-swap or fencing operation it cannot exclude two starts
  that both observe a free file. Host serialization is a requirement, not a
  guarantee supplied by this lease. Failed reads/renewals or an expired own
  lease stop the holder; damaged records cannot be treated as a free database.
- **An outcome nobody knows is looked up, not assumed.** A commit PUT that
  failed or lost its reply used to end the generation, so one 503 from the
  object store cost a copy of the whole database. The marker now says which
  sequence is uncertain; the next sync lists it and counts it or retries it.
  A stop between a commit and its marker write is the same question at start:
  the bucket's tip is past the marker, the dirty log still names every page
  since the marker, and the generation continues at the tip.
- **A renewal never takes the generation it renews with it.** Until the new
  generation's first commit the dirty log follows the old one, and the marker
  keeps the old one as `Previous`. A renewal that fails goes back to it, with
  only the pages that changed since its last commit; a stop in the middle of
  one continues it at the next start. The renewal is tried again after ten
  minutes, and meanwhile changes keep shipping.

`TestCrashMatrix` stops a sync at each of these places (a segment, a commit,
a lost reply, the marker, the dirty log, the clean marker, compaction, and the
same for a renewal), once followed by a retry in the same process and once by a
crash and a start. Each time the database keeps every write, nothing is
restored over it, no whole-database copy follows, and after one more sync the
bucket restores exactly what the database holds.

## The failure that produced the bold rows

A database of 8.47 GB was replicated correctly for days. At 20:50 UTC its size
dropped to 4.57 GB, which started a new generation with a fresh, complete
snapshot of 4.58 GB. The database then grew back to 8.25 GB, and for that
growth the replica shipped 16 MB. It kept committing for ten more hours.

Every check passed: 19 manifests, 320 parts, all present, all with the right
size and hash, sequence chain 1..32 closed. But a restore truncates to the size
the manifest records and hands the file to SQLite, so it produced an 8.25 GB
file with 3.7 GB of holes, and SQLite said `database disk image is malformed`.
The one number nobody compared was the size against the pages.

Then the recovery made it worse, for a reason worth keeping in writing: a
database created from nothing was adopted as the truth because a file that
exists always won, its fresh snapshot became the current generation, and the
real data sat one pointer away in the bucket. Storage was never the problem.
Provenance was.

## Where each new guard sits

**The commit gate** (`growthGap`, called in `sync`). A growing database writes
its new pages, so they are dirty and the capture holds them. If the pages
between the previous commit's size and this one are not in the capture, the
tracker never saw those writes and no later increment will bring them. The
commit does not happen, the generation stays restorable as it is, the log says
how many pages are missing and which one is first, and the next sync starts a
fresh generation. Conservative on purpose: a page an earlier commit shipped and
that a shrink-then-grow left untouched also counts, which costs one snapshot
too many. Cheap, against a generation that can never restore.

**The source guard** (`checkSource`, before every capture). Two questions in
one read transaction. Does the page shipped last still hold the bytes that were
shipped for it? That is Litestream's `lastPageMatch`, and a page the tracker
holds as dirty is skipped because a legitimate rewrite is what a sync is for.
And did SQLite's file change counter (page 1, offset 24) move while the dirty
set stayed empty? Every write transaction touches page 1, so a counter that
moved without a mark means the write went around the VFS. On a hit it says so
once, marks the marker incomplete, and clears its own witness, because the
fresh generation is the remedy and a guard that refuses its own repair is
worse than no guard.

The tracker's page size and dirty set must also be read **inside** that read
transaction. Through v1.29.34 the dirty count was taken before acquiring it.
A tracked write finishing while the guard waited for the connection therefore
paired an old empty dirty set with a new file change counter, falsely reporting
an outside write and forcing a full generation copy. The same race could use
an old page size after `VACUUM`. `TestSourceGuardTrackedWritesAtReadBoundaries`
reproduces the ordering without timing assumptions, with both 4 KiB and 64 KiB
pages, and checks writes around capture and upload plus an integrity-checked
restore. `TestSourceGuardWaitsForWriter` also holds the actual SQLite connection
until the guard is waiting, then commits, rolls back spilled pages, or cancels
the wait. The adjacent tests cover a page size change and a real untracked
commit in that same window; the latter must still invalidate the generation.

**Provenance at open** (`adoptLocal`, in `Prepare`). The local marker names the
bucket's generation and is at or past its tip: it continues. It is behind that
tip: those commits were this process's own (the lease makes it the only
writer), so it continues at the tip with what the dirty log names, or starts a
fresh generation from the file when the log cannot say; it never restores over
the file, which is what Litestream's `checkDatabaseBehindReplica` does too. The
bucket moved to a generation this marker never made: a file with nothing
unshipped is a stale copy and the bucket wins, a file with writes the bucket
lacks refuses. The file has no replica state at all while the bucket holds a
generation: nothing orders the two, so neither is buried. It refuses, names
both, and the operator chooses. `AdoptLocalDatabase` is the opt-in for "this
file is the new truth", and it logs that it was used.

**Coverage at restore** (`pageSet.shortfall`). While composing, the restore
records every page it ever wrote. A page the generation never carried is a
hole, and the error names the counts instead of leaving SQLite to call the
result malformed. A page that *was* shipped and that a later truncation cut
away is explicitly fine: the source has the same zero there after it grew
again, and `TestCompactionPreservesShrinkThenGrow` guards exactly that.

**The host's verdict** (`Options.Verify`). Before a restore is published over a
database that may still be serving, the host runs `PRAGMA quick_check` on the
scratch file. Coverage proves the pages are all there; this proves they compose
into something SQLite will open. Spin wires it to
`persistence.QuickCheck`; this package stays free of a SQLite driver.

## Litestream, side by side

Litestream solves the same problem through the WAL. Its guards live in `db.go`:
`acquireReadLock`, `verify` on the WAL salts, `lastPageMatch`,
`detectFullCheckpoint`, `checkDatabaseBehindReplica`, and `IntegrityCheck` in
the restore path.

| Litestream | Here |
| --- | --- |
| Read lock so nobody checkpoints behind it | Not possible for us: we watch writes, we do not own the WAL. The change counter is the substitute detector |
| `verify` on WAL header salts | The dirty log plus the clean/complete marker; a surprise ends the generation |
| `lastPageMatch` | `checkSource`, same idea on the page shipped last |
| `detectFullCheckpoint` | Not applicable: rollback-journal mode, no checkpoints |
| `checkDatabaseBehindReplica` | `adoptLocal`, plus the refusal for a file with no provenance |
| The lease (`lock.json`, conditional PUT) | `lease.go`, next to the database on the shared volume |
| A snapshot streamed next to the incremental line | A renewal in short transactions with `Previous` to fall back on; while it copies, increments wait (see below) |
| `IntegrityCheckQuick` on restore | `Options.Verify` |
| Per-file checksums: not on every object | Size and sha256 on every part, verified on every read |
| Generations, snapshots, retention | The same, plus tiered windows so points thin out instead of disappearing |

Two places where we are stronger, and one where we are weaker. Weaker: we
depend on seeing every write, and the change counter catches that a write was
missed, not which pages it touched, so the remedy is always a fresh snapshot.
Stronger: every byte in the bucket is checksummed and every commit is atomic.

## What is still open

- The host must enforce one writer per local file **and** bucket namespace.
  The lease is not a distributed lock: two simultaneous starts can both see
  it free, and no storage primitive fences an old process after a long pause.
  Stronger protection requires an atomic lock/fencing primitive from the host.

- The final live-copy reconciliation excludes writers while it reads and
  spools the remaining dirty pages. Its page buffers are segment-bounded, but
  its duration depends on how much changed during the copy. This is the
  consistency boundary; writes after it belong to the next sync.

- While a renewal copies the whole database (for 8 GiB, tens of minutes) no
  increments ship; Litestream streams its snapshot beside them. A crash in that
  window loses nothing (the file and the dirty log hold it, and `Previous`
  continues), but the bucket lags for as long as the copy takes.

- The change counter only holds while the process runs. Persisting it in the
  marker would extend the guard across restarts, and that needs a format bump.
- A restore picks the generation `current` names. Choosing an older generation,
  which is what the 22 September recovery needed, is a manual pointer write. It
  belongs in the API.
- `growthGap` is conservative, so a shrink-then-grow inside one generation can
  cost an extra snapshot. Tracking per-generation page coverage in the marker
  would make it exact, at the price of state.
