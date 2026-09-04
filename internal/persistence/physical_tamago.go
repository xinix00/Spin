//go:build tamago

package persistence

import (
	"context"
	"errors"
	"io"
	"io/fs"
	"sync"

	"github.com/xinix00/HopOS/metal/v2/app/applib"
)

var physicalHopApp *applib.App

// ioChunkPool recycles the 1 MiB transfer buffers. Parallel restore chunks
// and backup streams would otherwise allocate one per call, and that garbage
// keeps the app's GC busy enough to starve the network pump on one core.
var ioChunkPool = sync.Pool{New: func() any {
	buffer := make([]byte, applib.MaxIOChunk)
	return &buffer
}}

func readPhysicalFile(ctx context.Context, path string, destination io.Writer) error {
	if physicalHopApp == nil {
		return errors.New("HopOS persistence is not registered")
	}
	size, err := physicalHopApp.Stat(path)
	if err != nil {
		return err
	}
	bufferPtr := ioChunkPool.Get().(*[]byte)
	defer ioChunkPool.Put(bufferPtr)
	buffer := *bufferPtr
	for offset := uint64(0); offset < size; {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}
		count := size - offset
		if count > uint64(len(buffer)) {
			count = uint64(len(buffer))
		}
		read, err := physicalHopApp.ReadInto(path, offset, buffer[:count])
		if err != nil {
			return err
		}
		if read == 0 {
			return io.ErrUnexpectedEOF
		}
		written, err := destination.Write(buffer[:read])
		if err != nil {
			return err
		}
		if written != read {
			return io.ErrShortWrite
		}
		offset += uint64(read)
	}
	return nil
}

func createPhysicalFile(path string) error {
	if physicalHopApp == nil {
		return errors.New("HopOS persistence is not registered")
	}
	return physicalHopApp.Truncate(path, 0)
}

func appendPhysicalFile(ctx context.Context, path string, source io.Reader, offset, maxBytes int64) (int64, error) {
	if physicalHopApp == nil {
		return 0, errors.New("HopOS persistence is not registered")
	}
	bufferPtr := ioChunkPool.Get().(*[]byte)
	defer ioChunkPool.Put(bufferPtr)
	buffer := *bufferPtr
	var total int64
	for {
		select {
		case <-ctx.Done():
			return total, ctx.Err()
		default:
		}
		remaining := maxBytes - total
		if remaining <= 0 {
			return total, nil
		}
		chunk := buffer
		if int64(len(chunk)) > remaining {
			chunk = chunk[:remaining]
		}
		count, readErr := io.ReadFull(source, chunk)
		if count > 0 {
			written, err := physicalHopApp.WriteAt(path, uint64(offset+total), chunk[:count])
			if err != nil {
				return total, err
			}
			if written != count {
				return total, io.ErrShortWrite
			}
			total += int64(count)
		}
		if readErr != nil {
			if errors.Is(readErr, io.EOF) || errors.Is(readErr, io.ErrUnexpectedEOF) {
				return total, nil
			}
			return total, readErr
		}
	}
}

func writePhysicalFile(ctx context.Context, path string, source io.Reader, maxBytes int64) error {
	if err := createPhysicalFile(path); err != nil {
		return err
	}
	written, copyErr := appendPhysicalFile(ctx, path, source, 0, maxBytes+1)
	if written > maxBytes {
		return errors.Join(errors.New("backup exceeds upload limit"), copyErr)
	}
	return copyErr
}

func removePhysicalFile(path string) error {
	if physicalHopApp == nil {
		return nil
	}
	err := physicalHopApp.Remove(path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil
	}
	return err
}
