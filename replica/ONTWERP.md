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
| A gap in the commit sequence | Compaction refuses to merge across it |
| **The database grows by pages this replica never saw** | **The commit gate: `growthGap`, `coverage.go`** |
| **A write that went around the tracking VFS** | **The source guard: `checkSource`, `guard.go`** |
| **A database that lost writes winning over the replica** | **`adoptLocal`, `guard.go`** |
| **A composition that is short of the size it records** | **`pageSet.shortfall`, `coverage.go`** |
| **A composed file SQLite would reject** | **`Options.Verify`, the host's `PRAGMA quick_check`** |

The five in bold are new since 22 September. The four rows above them were
there all along and held; they are why the bucket was intact while the data was
still unreachable.

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

**Provenance at open** (`adoptLocal`, in `Prepare`). Three outcomes. The local
marker names the bucket's generation and is at or past its tip: it continues.
The marker is behind that tip: the replica is provably ahead and wins, which is
Litestream's `checkDatabaseBehindReplica`. The file has no replica state at all
while the bucket holds a generation: nothing orders the two, so neither is
buried. It refuses, names both, and the operator chooses. `AdoptLocalDatabase`
is the opt-in for "this file is the new truth", and it logs that it was used.

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
| `IntegrityCheckQuick` on restore | `Options.Verify` |
| Per-file checksums: not on every object | Size and sha256 on every part, verified on every read |
| Generations, snapshots, retention | The same, plus tiered windows so points thin out instead of disappearing |

Two places where we are stronger, and one where we are weaker. Weaker: we
depend on seeing every write, and the change counter catches that a write was
missed, not which pages it touched, so the remedy is always a fresh snapshot.
Stronger: every byte in the bucket is checksummed and every commit is atomic.

## What is still open

- The change counter only holds while the process runs. Persisting it in the
  marker would extend the guard across restarts, and that needs a format bump.
- A restore picks the generation `current` names. Choosing an older generation,
  which is what the 22 September recovery needed, is a manual pointer write. It
  belongs in the API.
- `growthGap` is conservative, so a shrink-then-grow inside one generation can
  cost an extra snapshot. Tracking per-generation page coverage in the marker
  would make it exact, at the price of state.
