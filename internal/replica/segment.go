package replica

import (
	"bytes"
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"fmt"
)

// A segment is a set of database pages as they were at one consistent
// moment, with the database size at that moment. Applying the segments of a
// generation in order reproduces the database; the first segments of a
// generation together hold every page (the snapshot), the later ones only
// what changed.
//
//	SPINSEG1 | page size u32 | database size u64 | count u32
//	count × (page number u32 | page bytes)
//	sha256 of everything above

const segmentMagic = "SPINSEG1"

type segment struct {
	PageSize int
	DBSize   int64
	Pages    []uint32
	Data     [][]byte
}

func encodeSegment(segment segment) []byte {
	var buffer bytes.Buffer
	buffer.Grow(8 + 4 + 8 + 4 + len(segment.Pages)*(4+segment.PageSize) + 32)
	buffer.WriteString(segmentMagic)
	var header [16]byte
	binary.BigEndian.PutUint32(header[0:4], uint32(segment.PageSize))
	binary.BigEndian.PutUint64(header[4:12], uint64(segment.DBSize))
	binary.BigEndian.PutUint32(header[12:16], uint32(len(segment.Pages)))
	buffer.Write(header[:])
	var number [4]byte
	for index, page := range segment.Pages {
		binary.BigEndian.PutUint32(number[:], page)
		buffer.Write(number[:])
		buffer.Write(segment.Data[index])
	}
	sum := sha256.Sum256(buffer.Bytes())
	buffer.Write(sum[:])
	return buffer.Bytes()
}

func decodeSegment(data []byte) (segment, error) {
	if len(data) < 8+16+32 || string(data[:8]) != segmentMagic {
		return segment{}, errors.New("not a Spin segment")
	}
	body, trailer := data[:len(data)-32], data[len(data)-32:]
	if sum := sha256.Sum256(body); !bytes.Equal(sum[:], trailer) {
		return segment{}, errors.New("segment checksum mismatch")
	}
	header := body[8:24]
	out := segment{
		PageSize: int(binary.BigEndian.Uint32(header[0:4])),
		DBSize:   int64(binary.BigEndian.Uint64(header[4:12])),
	}
	count := int(binary.BigEndian.Uint32(header[12:16]))
	if out.PageSize < 512 || out.PageSize > 65536 {
		return segment{}, fmt.Errorf("segment page size %d", out.PageSize)
	}
	rest := body[24:]
	if len(rest) != count*(4+out.PageSize) {
		return segment{}, fmt.Errorf("segment holds %d bytes for %d pages of %d", len(rest), count, out.PageSize)
	}
	out.Pages = make([]uint32, count)
	out.Data = make([][]byte, count)
	for index := 0; index < count; index++ {
		record := rest[index*(4+out.PageSize):]
		out.Pages[index] = binary.BigEndian.Uint32(record[:4])
		out.Data[index] = record[4 : 4+out.PageSize]
	}
	return out, nil
}
