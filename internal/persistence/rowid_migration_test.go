package persistence

import (
	"context"
	"database/sql"
	"path/filepath"
	"strings"
	"testing"
)

// The schema before v1.7.6 kept spin_kv and spin_object_chunks WITHOUT ROWID.
// Open must rebuild them as rowid tables and keep every row.
func TestOpenRebuildsWithoutRowidTables(t *testing.T) {
	path := filepath.Join(t.TempDir(), "spin.db")
	legacy, err := sql.Open("sqlite3", sqliteDSN(path, ""))
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	for _, statement := range []string{
		`CREATE TABLE spin_kv (key TEXT PRIMARY KEY, value BLOB NOT NULL) WITHOUT ROWID`,
		`CREATE TABLE spin_objects (id INTEGER PRIMARY KEY, digest TEXT, kind TEXT NOT NULL, size INTEGER NOT NULL DEFAULT 0, complete INTEGER NOT NULL DEFAULT 0)`,
		`CREATE TABLE spin_object_chunks (
			object_id INTEGER NOT NULL REFERENCES spin_objects(id) ON DELETE CASCADE,
			sequence INTEGER NOT NULL, data BLOB NOT NULL,
			PRIMARY KEY(object_id, sequence)) WITHOUT ROWID`,
		`INSERT INTO spin_kv(key, value) VALUES('state', x'0102'), ('other', x'03')`,
		`INSERT INTO spin_objects(id, kind, size, complete) VALUES(1, 'docker-snapshot', 3, 1)`,
		`INSERT INTO spin_object_chunks(object_id, sequence, data) VALUES(1, 1, x'bb'), (1, 0, x'aa'), (1, 2, x'cc')`,
	} {
		if _, err := legacy.ExecContext(ctx, statement); err != nil {
			t.Fatalf("%s: %v", statement, err)
		}
	}
	if err := legacy.Close(); err != nil {
		t.Fatal(err)
	}

	database, err := Open(path, OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer database.Close()
	if got := database.Migrations(); len(got) != 2 || !strings.Contains(got[0], "spin_kv") || !strings.Contains(got[1], "spin_object_chunks") {
		t.Fatalf("migrations = %q", got)
	}
	for _, table := range []string{"spin_kv", "spin_object_chunks"} {
		var definition string
		if err := database.db.QueryRowContext(ctx, `SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?`, table).Scan(&definition); err != nil {
			t.Fatalf("%s: %v", table, err)
		}
		if strings.Contains(strings.ToUpper(definition), "WITHOUT ROWID") {
			t.Fatalf("%s still WITHOUT ROWID: %s", table, definition)
		}
	}
	if value, err := database.ReadFile("state"); err != nil || string(value) != "\x01\x02" {
		t.Fatalf("state after rebuild = %q, %v", value, err)
	}
	var sequences string
	if err := database.db.QueryRowContext(ctx, `SELECT group_concat(sequence || ':' || hex(data), ',') FROM (SELECT sequence, data FROM spin_object_chunks WHERE object_id = 1 ORDER BY sequence)`).Scan(&sequences); err != nil {
		t.Fatal(err)
	}
	if sequences != "0:AA,1:BB,2:CC" {
		t.Fatalf("chunks after rebuild = %s", sequences)
	}
	// The unique key still makes a retried chunk a plain replace.
	if _, err := database.db.ExecContext(ctx, `INSERT OR REPLACE INTO spin_object_chunks(object_id, sequence, data) VALUES(1, 1, x'dd')`); err != nil {
		t.Fatal(err)
	}
	var count int
	if err := database.db.QueryRowContext(ctx, `SELECT count(*) FROM spin_object_chunks WHERE object_id = 1`).Scan(&count); err != nil || count != 3 {
		t.Fatalf("count after replace = %d, %v", count, err)
	}
	var foreignKeys int
	if err := database.db.QueryRowContext(ctx, `PRAGMA foreign_keys`).Scan(&foreignKeys); err != nil || foreignKeys != 1 {
		t.Fatalf("foreign_keys after rebuild = %d, %v", foreignKeys, err)
	}

	// A second Open finds nothing to rebuild.
	database.Close()
	again, err := Open(path, OpenOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer again.Close()
	if got := again.Migrations(); len(got) != 0 {
		t.Fatalf("second open migrated again: %q", got)
	}
}
