//go:build !tamago

package persistence

import (
	"context"
	"errors"
	"io"
	"os"
	"path/filepath"
)

func readPhysicalFile(ctx context.Context, path string, destination io.Writer) error {
	file, err := os.Open(path)
	if err != nil {
		return err
	}
	defer file.Close()
	_, err = io.Copy(destination, &contextReader{ctx: ctx, reader: file})
	return err
}

func createPhysicalFile(path string) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o750); err != nil {
		return err
	}
	file, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if err != nil {
		return err
	}
	return file.Close()
}

func appendPhysicalFile(ctx context.Context, path string, source io.Reader, offset, maxBytes int64) (int64, error) {
	file, err := os.OpenFile(path, os.O_WRONLY, 0o600)
	if err != nil {
		return 0, err
	}
	if _, err := file.Seek(offset, io.SeekStart); err != nil {
		_ = file.Close()
		return 0, err
	}
	written, copyErr := io.Copy(file, io.LimitReader(&contextReader{ctx: ctx, reader: source}, maxBytes))
	return written, errors.Join(copyErr, file.Close())
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
	err := os.Remove(path)
	if os.IsNotExist(err) {
		return nil
	}
	return err
}

type contextReader struct {
	ctx    context.Context
	reader io.Reader
}

func (r *contextReader) Read(target []byte) (int, error) {
	select {
	case <-r.ctx.Done():
		return 0, r.ctx.Err()
	default:
		return r.reader.Read(target)
	}
}
