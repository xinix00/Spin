package replica

import "testing"

func FuzzDecodeSegment(f *testing.F) {
	f.Add(encodeSegment(segment{PageSize: 512, DBSize: 512, Pages: []uint32{1}, Data: [][]byte{make([]byte, 512)}}))
	f.Add([]byte("SPINSEG1"))
	f.Fuzz(func(t *testing.T, data []byte) {
		seg, err := decodeSegment(data)
		if err != nil {
			return
		}
		encoded := encodeSegment(seg)
		if _, err := decodeSegment(encoded); err != nil {
			t.Fatalf("decoded value cannot round trip: %v", err)
		}
	})
}
