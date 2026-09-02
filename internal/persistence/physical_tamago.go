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

func writePhysicalFile(ctx context.Context, path string, source io.Reader, maxBytes int64) error {
	if physicalHopApp == nil {
		return errors.New("HopOS persistence is not registered")
	}
	if err := physicalHopApp.Truncate(path, 0); err != nil {
		return err
	}
	buffer := make([]byte, hopabi.MaxChunk)
	var offset int64
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		default:
		}
		count, readErr := source.Read(buffer)
		if count > 0 {
			if offset+int64(count) > maxBytes {
				return errors.New("backup exceeds upload limit")
			}
			written, err := physicalHopApp.WriteAt(path, uint64(offset), buffer[:count])
			if err != nil {
				return err
			}
			if written != count {
				return io.ErrShortWrite
			}
			offset += int64(count)
		}
		if readErr != nil {
			if errors.Is(readErr, io.EOF) {
				return nil
			}
			return readErr
		}
	}
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
