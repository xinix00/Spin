//go:build tamago

package persistence

import (
	"context"
	"errors"
	"io"
	"io/fs"

	"github.com/xinix00/HopOS/metal/abi/hopabi"
	"github.com/xinix00/HopOS/metal/app/applib"
)

var physicalHopApp *applib.App

func readPhysicalFile(ctx context.Context, path string, destination io.Writer) error {
	if physicalHopApp == nil {
		return errors.New("HopOS persistence is not registered")
	}
	size, err := physicalHopApp.Stat(path)
	if err != nil {
		return err
	}
	for offset := uint64(0); offset < size; {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}
		count := int(size - offset)
		if count > hopabi.MaxChunk {
			count = hopabi.MaxChunk
		}
		chunk, err := physicalHopApp.ReadAt(path, offset, count)
		if err != nil {
			return err
		}
		if len(chunk) == 0 {
			return io.ErrUnexpectedEOF
		}
		written, err := destination.Write(chunk)
		if err != nil {
			return err
		}
		if written != len(chunk) {
			return io.ErrShortWrite
		}
		offset += uint64(len(chunk))
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
	buffer := make([]byte, hopabi.MaxChunk)
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
		count, readErr := source.Read(chunk)
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
			if errors.Is(readErr, io.EOF) {
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
