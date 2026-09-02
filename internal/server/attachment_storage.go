package server

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// AttachmentStorage is the small persistence seam shared by Linux and HopOS.
// Names are opaque single path components; Server validates the attachment ID
// before it reaches this interface.
type AttachmentStorage interface {
	ReadFile(name string) ([]byte, error)
	WriteFile(name string, data []byte) error
	Remove(name string) error
	// LocalPath is non-empty only when another local process can read the file
	// directly. HopOS returns an empty path and sends bytes to remote runners.
	LocalPath(name string) string
}

type filesystemAttachmentStorage struct{ directory string }

func newFilesystemAttachmentStorage(directory string) AttachmentStorage {
	directory = strings.TrimSpace(directory)
	if directory == "" {
		return nil
	}
	return filesystemAttachmentStorage{directory: directory}
}

func (s filesystemAttachmentStorage) path(name string) (string, error) {
	if name == "" || filepath.Base(name) != name || name == "." {
		return "", fmt.Errorf("invalid attachment storage name %q", name)
	}
	return filepath.Join(s.directory, name), nil
}

func (s filesystemAttachmentStorage) ReadFile(name string) ([]byte, error) {
	path, err := s.path(name)
	if err != nil {
		return nil, err
	}
	return os.ReadFile(path)
}

func (s filesystemAttachmentStorage) WriteFile(name string, data []byte) error {
	path, err := s.path(name)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(s.directory, 0o750); err != nil {
		return fmt.Errorf("create attachment directory: %w", err)
	}
	temporary, err := os.CreateTemp(s.directory, ".spin-attachment-*")
	if err != nil {
		return fmt.Errorf("create attachment temp file: %w", err)
	}
	temporaryPath := temporary.Name()
	defer os.Remove(temporaryPath)
	if _, err := temporary.Write(data); err != nil {
		_ = temporary.Close()
		return fmt.Errorf("write attachment: %w", err)
	}
	if err := temporary.Sync(); err != nil {
		_ = temporary.Close()
		return fmt.Errorf("sync attachment: %w", err)
	}
	if err := temporary.Chmod(0o600); err != nil {
		_ = temporary.Close()
		return err
	}
	if err := temporary.Close(); err != nil {
		return err
	}
	if err := os.Rename(temporaryPath, path); err != nil {
		return fmt.Errorf("publish attachment: %w", err)
	}
	return nil
}

func (s filesystemAttachmentStorage) Remove(name string) error {
	path, err := s.path(name)
	if err != nil {
		return err
	}
	if err := os.Remove(path); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	return nil
}

func (s filesystemAttachmentStorage) LocalPath(name string) string {
	path, err := s.path(name)
	if err != nil {
		return ""
	}
	return path
}
