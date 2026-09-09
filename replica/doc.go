// Package replica replicates a SQLite database through a tracking VFS, with
// atomic snapshot and incremental commit manifests and tiered restore points.
// It depends on ncruces/go-sqlite3's VFS interfaces, not on an application's
// schema, HTTP server, environment variables, or lifecycle framework.
//
// The host must use rollback-journal mode and exclude all writers while its
// Database.WithReadTransaction callback runs. Local Storage and the wrapped
// VFS must access the same bytes. Each object-store namespace has one writer.
// Object replacement and listings must be strongly consistent. All SQLite
// database writes must pass through VFSName; direct file writes are unsupported.
//
// Sync captures one complete state into a local spool before uploading, in
// short read transactions so the database stays usable meanwhile. It
// bounds page data in memory by SegmentBytes, plus dirty-page/part metadata,
// and requires local scratch space for the captured pages. Readers only follow
// published manifests. The generation's full snapshot is retained separately
// from incremental windows until the generation itself expires.
package replica
