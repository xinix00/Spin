package replica

import (
	"encoding/binary"
	"fmt"
	"io"
	"slices"
	"strings"
	"testing"
	"time"
)

type dirtyLogReadFile struct {
	File
	read func([]byte, int64) (int, error)
}

func (f dirtyLogReadFile) ReadAt(data []byte, offset int64) (int, error) {
	return f.read(data, offset)
}

func dirtyLogTestPages(count int) []uint32 {
	pages := make([]uint32, count)
	for i := range pages {
		pages[i] = uint32(i + 1)
	}
	return pages
}

// Both rewriting and appending a large dirty set must bound the encoded
// buffer; replay only needs the returned page numbers and one input chunk.
func TestDirtyLogStreamsLargePageSets(t *testing.T) {
	const generation = "chunked-log"
	pages := dirtyLogTestPages(4*dirtyLogChunkBytes/dirtyLogRecord + 3)
	var maxRead, maxWrite int
	var recordsSynced, headerWritten bool
	storage := faultStorage{Storage: OSStorage(), wrap: func(_ string, file File) File {
		return dirtyLogReadFile{File: faultFile{File: file, write: func(data []byte, offset int64) (int, error) {
			maxWrite = max(maxWrite, len(data))
			if offset == 0 {
				if !recordsSynced {
					t.Fatal("header written before the records became durable")
				}
				headerWritten = true
			}
			return file.WriteAt(data, offset)
		}, sync: func() error {
			if !headerWritten {
				size, err := file.Size()
				if err != nil || size != int64(dirtyLogHeaderSize(generation)+len(pages)*dirtyLogRecord) {
					t.Fatalf("records sync before the complete body: size=%d, err=%v", size, err)
				}
				recordsSynced = true
			}
			return file.Sync()
		}}, read: func(data []byte, offset int64) (int, error) {
			maxRead = max(maxRead, len(data))
			return file.ReadAt(data, offset)
		}}
	}}
	log := newDirtyLog(storage, t.TempDir()+"/large.db")
	defer log.close()
	if err := log.rewrite(generation, 7, pages); err != nil {
		t.Fatal(err)
	}
	for _, page := range pages {
		log.note(page + uint32(len(pages)))
	}
	if err := log.flush(); err != nil {
		t.Fatal(err)
	}
	gotGeneration, seq, got, err := log.readFile(log.paths[log.current])
	want := dirtyLogTestPages(2 * len(pages))
	if err != nil || gotGeneration != generation || seq != 7 || !slices.Equal(got, want) {
		t.Fatalf("large log round trip: generation=%q, seq=%d, pages=%d, err=%v", gotGeneration, seq, len(got), err)
	}
	if maxRead > dirtyLogChunkBytes || maxWrite > dirtyLogChunkBytes {
		t.Fatalf("unbounded log I/O: read=%d, write=%d", maxRead, maxWrite)
	}
}

func TestDirtyLogChunkAppendFailureRetriesWithoutDroppingPages(t *testing.T) {
	for _, failure := range []string{"short-write", "partial-error", "lost-write-reply", "lost-sync-reply"} {
		t.Run(failure, func(t *testing.T) {
			log := newDirtyLog(OSStorage(), t.TempDir()+"/append.db")
			defer log.close()
			if err := log.rewrite("generation", 2, []uint32{1}); err != nil {
				t.Fatal(err)
			}
			pages := dirtyLogTestPages(3*dirtyLogChunkBytes/dirtyLogRecord + 1)
			for _, page := range pages[1:] {
				log.note(page)
			}
			file, oldSize := log.file, log.size
			writes, failed := 0, false
			log.file = faultFile{File: file, write: func(data []byte, offset int64) (int, error) {
				writes++
				if writes != 2 || failure == "lost-sync-reply" {
					return file.WriteAt(data, offset)
				}
				failed = true
				count := len(data)
				if failure != "lost-write-reply" {
					count--
				}
				n, err := file.WriteAt(data[:count], offset)
				if err != nil || failure == "short-write" {
					return n, err
				}
				return n, errInjected
			}, sync: func() error {
				if err := file.Sync(); err != nil {
					return err
				}
				failed = true
				return errInjected
			}}
			if err := log.flush(); err == nil || !failed {
				t.Fatalf("flush did not fail as intended: %v", err)
			}
			if log.size != oldSize || !slices.Equal(log.pending, pages[1:]) {
				t.Fatal("failed append discarded its retry state")
			}
			log.file = file
			if err := log.flush(); err != nil {
				t.Fatal(err)
			}
			_, _, got, err := log.readFile(log.paths[log.current])
			if err != nil || !slices.Equal(got, pages) {
				t.Fatalf("retry lost or duplicated pages: got=%d, want=%d, err=%v", len(got), len(pages), err)
			}
		})
	}
}

func TestDirtyLogStreamsMaximumGenerationHeader(t *testing.T) {
	log := newDirtyLog(OSStorage(), t.TempDir()+"/header.db")
	defer log.close()
	generation := strings.Repeat("g", 65535)
	if err := log.rewrite(generation, 3, []uint32{1}); err != nil {
		t.Fatal(err)
	}
	log.files = faultStorage{Storage: OSStorage(), wrap: func(_ string, file File) File {
		return dirtyLogReadFile{File: file, read: func(data []byte, offset int64) (int, error) {
			if len(data) > dirtyLogChunkBytes {
				t.Fatalf("header read exceeds chunk size: %d", len(data))
			}
			return file.ReadAt(data, offset)
		}}
	}}
	gotGeneration, seq, pages, err := log.readFile(log.paths[log.current])
	if err != nil || gotGeneration != generation || seq != 3 || !slices.Equal(pages, []uint32{1}) {
		t.Fatalf("maximum-size header round trip: generation bytes=%d, seq=%d, pages=%v, err=%v", len(gotGeneration), seq, pages, err)
	}
}

func TestDirtyLogRejectsShortReadsAndKeepsTornAppendRule(t *testing.T) {
	log := newDirtyLog(OSStorage(), t.TempDir()+"/read.db")
	defer log.close()
	pages := dirtyLogTestPages(dirtyLogChunkBytes/dirtyLogRecord + 1)
	// Its complement consists of zeroes. A silently zero-filled short read
	// would otherwise make this last record look complete and valid.
	pages[len(pages)-1] = ^uint32(0)
	if err := log.rewrite("generation", 1, pages); err != nil {
		t.Fatal(err)
	}
	path := log.paths[log.current]
	headerSize := int64(dirtyLogHeaderSize("generation"))
	for _, offset := range []int64{0, int64(dirtyLogFixedHeader), headerSize, headerSize + dirtyLogChunkBytes} {
		for _, failure := range []string{"short-nil", "short-eof", "read-error", "full-eof"} {
			t.Run(fmt.Sprintf("%s/offset-%d", failure, offset), func(t *testing.T) {
				fired := false
				log.files = faultStorage{Storage: OSStorage(), wrap: func(_ string, file File) File {
					return dirtyLogReadFile{File: file, read: func(data []byte, at int64) (int, error) {
						if at != offset {
							return file.ReadAt(data, at)
						}
						fired = true
						if failure == "read-error" {
							return 0, errInjected
						}
						count := len(data)
						if failure != "full-eof" {
							count--
						}
						n, err := file.ReadAt(data[:count], at)
						if err != nil || failure == "short-nil" {
							return n, err
						}
						return n, io.EOF
					}}
				}}
				_, _, got, err := log.readFile(path)
				if !fired {
					t.Fatal("short read was not injected")
				}
				if failure == "full-eof" {
					if err != nil || !slices.Equal(got, pages) {
						t.Fatalf("complete read with EOF was refused: %v", err)
					}
				} else if err == nil {
					t.Fatal("accepted an incomplete or failed read")
				}
			})
		}
	}
	log.files = OSStorage()
	for trailing := 1; trailing < dirtyLogRecord; trailing++ {
		if _, err := log.file.WriteAt(make([]byte, trailing), log.size); err != nil {
			t.Fatal(err)
		}
		_, _, got, err := log.readFile(path)
		if err != nil || !slices.Equal(got, pages) {
			t.Fatalf("torn final record (%d bytes): %v", trailing, err)
		}
	}
	var broken [dirtyLogRecord]byte
	binary.BigEndian.PutUint32(broken[4:], ^uint32(0))
	if _, err := log.file.WriteAt(broken[:], log.size); err != nil {
		t.Fatal(err)
	}
	if _, _, _, err := log.readFile(path); err == nil {
		t.Fatal("accepted an unplaceable-write record after a chunk boundary")
	}
}

// A published header can survive an error returned by WriteAt or Sync.
// Subsequent database writes must never go only into the older log, since
// recovery would then select the newer, incomplete page set.
func TestDirtyLogUncertainRewriteCannotHideLaterWrites(t *testing.T) {
	for _, failure := range []string{"header-no-write", "header-short-write", "header-partial-error", "header-lost-write-reply", "header-sync-error", "header-lost-sync-reply"} {
		t.Run(failure, func(t *testing.T) {
			f := newFixture(t)
			f.write(t, []byte("one"))
			f.sync(t)
			f.write(t, []byte("two"))
			fired := false
			log := f.rep.tracker.log
			log.files = faultStorage{Storage: OSStorage(), wrap: func(path string, file File) File {
				if !strings.Contains(path, ".replica-dirty-") {
					return file
				}
				return faultFile{File: file, write: func(data []byte, offset int64) (int, error) {
					if fired || offset != 0 || strings.Contains(failure, "sync") {
						return file.WriteAt(data, offset)
					}
					fired = true
					if failure == "header-no-write" {
						return 0, errInjected
					}
					count := len(data)
					if failure == "header-short-write" {
						count--
					} else if failure == "header-partial-error" {
						count /= 2
					}
					n, err := file.WriteAt(data[:count], offset)
					if err != nil || failure == "header-short-write" {
						return n, err
					}
					return n, errInjected
				}, sync: func() error {
					if fired || !strings.Contains(failure, "sync") {
						return file.Sync()
					}
					fired = true
					if failure == "header-lost-sync-reply" {
						if err := file.Sync(); err != nil {
							return err
						}
					}
					return errInjected
				}}
			}}
			f.sync(t) // The bucket commit succeeds; local log rewrite warns.
			if !fired {
				t.Fatal("rewrite fault was not injected")
			}
			log.files = OSStorage()
			f.write(t, []byte("three"))
			restartReviewFixture(t, f)
			if !f.rep.SnapshotDue() && f.rep.Status().PendingPages == 0 {
				t.Fatal("restart silently lost writes after the uncertain rewrite")
			}
			f.sync(t)
			f.check(t, f.rep.getMarker().Generation, time.Time{}, []byte("three"))
		})
	}
}

func TestDirtyLogFlushWithoutPrepareKeepsUnplaceableWrites(t *testing.T) {
	log := newDirtyLog(OSStorage(), t.TempDir()+"/unprepared.db")
	defer log.close()
	log.note(7)
	log.markBroken()
	if err := log.flush(); err != nil {
		t.Fatal(err)
	}
	if !log.broken || len(log.noted) != 1 {
		t.Fatal("initialization discarded the tracked pages or broken state")
	}
	if _, _, _, err := log.readFile(log.paths[log.current]); err == nil {
		t.Fatal("initialization hid an unplaceable write")
	}
}

func TestDirtyLogRecordRewriteFailureKeepsPreviousLog(t *testing.T) {
	for _, failure := range []string{"short-second-chunk", "records-sync"} {
		t.Run(failure, func(t *testing.T) {
			log := newDirtyLog(OSStorage(), t.TempDir()+"/rewrite.db")
			defer log.close()
			if err := log.rewrite("generation", 1, []uint32{1}); err != nil {
				t.Fatal(err)
			}
			oldIndex, oldSize := log.current, log.size
			fired := false
			log.files = faultStorage{Storage: OSStorage(), wrap: func(_ string, file File) File {
				return faultFile{File: file, write: func(data []byte, offset int64) (int, error) {
					if failure == "short-second-chunk" && offset == int64(dirtyLogHeaderSize("generation"))+dirtyLogChunkBytes {
						fired = true
						return file.WriteAt(data[:len(data)-1], offset)
					}
					return file.WriteAt(data, offset)
				}, sync: func() error {
					fired = true
					return errInjected
				}}
			}}
			if err := log.rewrite("generation", 2, dirtyLogTestPages(3*dirtyLogChunkBytes/dirtyLogRecord)); err == nil || !fired {
				t.Fatalf("record rewrite did not fail: %v", err)
			}
			if log.current != oldIndex || log.size != oldSize || len(log.noted) != 1 {
				t.Fatal("record failure replaced the active log before the new header")
			}
			log.files = OSStorage()
			log.note(9)
			if err := log.flush(); err != nil {
				t.Fatal(err)
			}
			_, seq, got, err := log.readFile(log.paths[oldIndex])
			if err != nil || seq != 1 || !slices.Equal(got, []uint32{1, 9}) {
				t.Fatalf("previous log lost later writes: seq=%d, pages=%v, err=%v", seq, got, err)
			}
		})
	}
}

func BenchmarkDirtyLogReadLarge(b *testing.B) {
	log := newDirtyLog(OSStorage(), b.TempDir()+"/large.db")
	defer log.close()
	const count = 1_000_000
	if err := log.rewrite("generation", 1, dirtyLogTestPages(count)); err != nil {
		b.Fatal(err)
	}
	b.SetBytes(count * dirtyLogRecord)
	b.ReportAllocs()
	b.ResetTimer()
	for b.Loop() {
		_, _, pages, err := log.readFile(log.paths[log.current])
		if err != nil || len(pages) != count {
			b.Fatalf("read %d pages: %v", len(pages), err)
		}
	}
}
