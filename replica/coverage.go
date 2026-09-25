package replica

import "errors"

// errGenerationShort stops a commit that would record a size the generation
// cannot fill. The sync returns it so the failure is visible in the log; the
// marker is left incomplete, which makes the next sync begin a fresh
// generation with a full snapshot. A restore returns it too, with the
// numbers, for a generation that never held every page below its size.
var errGenerationShort = errors.New("replica: pages are missing for the size the database reports; a fresh generation is needed")

// A restore truncates to the size a manifest records and hands the file to
// SQLite. That only holds when every page below that size was shipped: a
// page nobody ever wrote is a hole, SQLite reads its own page count from
// page 1, finds a zeroed page where a b-tree node belongs, and reports
// "database disk image is malformed" without naming the cause.
//
// Measured on 22 September on a live tenant, and it cost a day of data. The
// generation looked perfect: 19 manifests, 320 parts, every part present
// with the right size and hash, the sequence chain 1..32 closed. Yet every
// restore produced a malformed image. The numbers told the story: the
// snapshot carries 4.58 GB of pages for a database that was 4.575 GB at the
// time (complete), while the next window records a size of 8.25 GB with
// 16 MB of pages shipped for it. The replica then kept writing increment
// after increment for hours on a foundation that could never come back, and
// said nothing about it.
//
// Hence two gates, one per side:
//
//   - the writer never records a size its own chain cannot fill (growthGap)
//     and asks for a fresh generation instead;
//   - a restore refuses a composition that is missing pages the generation
//     never held (pageSet.shortfall), with the numbers in the error.
//
// The restore side asks one precise question: was this page ever shipped in
// this generation? A page that WAS shipped and that a later truncation cut
// away is a legitimate zero, because the source file has the same zero there
// after it grew again (failure_test.go guards that). A page no segment of
// the generation ever carried is the hole that ends in a malformed image.

// pageSet remembers which pages a generation shipped. A bitmap: two
// million pages, an 8 GB database with 4 KB pages, costs 256 KB, and a slot
// on the node has that room.
type pageSet struct {
	bits []uint64
}

func (s *pageSet) add(page uint32) {
	if page == 0 {
		return
	}
	index := int(page-1) / 64
	for len(s.bits) <= index {
		s.bits = append(s.bits, 0)
	}
	s.bits[index] |= 1 << uint((page-1)%64)
}

func (s *pageSet) has(page uint32) bool {
	if page == 0 {
		return false
	}
	index := int(page-1) / 64
	if index >= len(s.bits) {
		return false
	}
	return s.bits[index]&(1<<uint((page-1)%64)) != 0
}

// shortfall names the first page below size that no segment ever carried,
// and how many there are. ok is false when pages are missing.
func (s *pageSet) shortfall(size int64, pageSize int) (first uint32, missing int64, ok bool) {
	if pageSize <= 0 || size <= 0 {
		return 0, 0, true
	}
	pages := size / int64(pageSize)
	lockPage := int64(1<<30)/int64(pageSize) + 1
	for page := int64(1); page <= pages; page++ {
		if page != lockPage && !s.has(uint32(page)) {
			if missing == 0 {
				first = uint32(page)
			}
			missing++
		}
	}
	return first, missing, missing == 0
}

// growthGap answers the writer's question: does this capture carry the pages
// the database grew by since the previous commit? A database that grows
// writes its new pages, so they are dirty and this capture holds them. When
// they are missing the tracker did not see those writes, and no later
// increment will bring them either.
//
// Conservative on purpose: a page an earlier commit of this generation
// already shipped, which a shrink and a later grow left untouched, also
// counts as a gap. That costs one snapshot too many, which is I/O; the other
// mistake costs a generation that cannot restore.
func growthGap(previous, size int64, pageSize int, pages []uint32) (first uint32, missing int64, gap bool) {
	if pageSize <= 0 || size <= previous {
		return 0, 0, false
	}
	from := previous/int64(pageSize) + 1
	through := size / int64(pageSize)
	// Index relative to the new tail: adding one page to a terabyte database
	// must not allocate a bitmap for all pages that are already in the bucket.
	have := pageSet{}
	for _, page := range pages {
		if int64(page) >= from && int64(page) <= through {
			have.add(uint32(int64(page) - from + 1))
		}
	}
	// SQLite never reads or writes its reserved lock-byte page. Growing past
	// 1 GiB therefore legitimately leaves this page absent from dirty tracking.
	// https://www.sqlite.org/fileformat.html#the_lock_byte_page
	lockPage := int64(1<<30)/int64(pageSize) + 1
	for page := from; page <= through; page++ {
		if page != lockPage && !have.has(uint32(page-from+1)) {
			if missing == 0 {
				first = uint32(page)
			}
			missing++
		}
	}
	return first, missing, missing > 0
}
