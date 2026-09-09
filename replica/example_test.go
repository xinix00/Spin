package replica_test

import (
	"context"
	"database/sql"
	"fmt"
	"net/url"
	"os"

	"easyacp/replica"
	_ "github.com/ncruces/go-sqlite3/driver"
	"github.com/ncruces/go-sqlite3/vfs"
)

// One connection serializes this application's writers with the callback.
// Reading sqlite_schema acquires a database read lock (SELECT 1 does not).
type database struct{ *sql.DB }

func (d database) WithReadTransaction(ctx context.Context, fn func() error) error {
	tx, err := d.BeginTx(ctx, &sql.TxOptions{ReadOnly: true})
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if _, err := tx.ExecContext(ctx, `SELECT count(*) FROM sqlite_schema`); err != nil {
		return err
	}
	return fn()
}

func ExampleNew() {
	// Values are supplied by the host; replica itself reads no environment.
	config := replica.Config{Endpoint: "https://s3.example.test", Bucket: "backups", AccessKey: "access", SecretKey: "secret"}
	dir, err := os.MkdirTemp("", "replica-example-")
	if err != nil {
		panic(err)
	}
	defer os.RemoveAll(dir)
	path := dir + "/app.db"
	rep, err := replica.New(config, "application", path, vfs.Find(""), nil)
	if err != nil {
		panic(err)
	}
	defer rep.Close()
	// In a real application, call rep.Prepare(ctx) here, before opening SQLite.
	u := url.URL{Scheme: "file", Path: path}
	q := url.Values{"vfs": {rep.VFSName()}}
	u.RawQuery = q.Encode()
	db, err := sql.Open("sqlite3", u.String())
	if err != nil {
		panic(err)
	}
	defer db.Close()
	db.SetMaxOpenConns(1)
	if _, err := db.Exec(`PRAGMA journal_mode=DELETE; CREATE TABLE items(id INTEGER PRIMARY KEY)`); err != nil {
		panic(err)
	}
	rep.Attach(database{db})
	// Use rep.Sync(ctx) for a manually driven scheduler, or rep.Start(ctx).
	fmt.Println(rep.Status().Enabled)
	// Output: true
}
