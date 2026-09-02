package security

import (
	"crypto/rand"
	"encoding/base64"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"time"
)

func LoadOrCreateToken(path string) (string, error) {
	path = strings.TrimSpace(path)
	if path == "" {
		return "", nil
	}
	if value, err := ReadToken(path); err == nil {
		return value, nil
	} else if !errors.Is(err, os.ErrNotExist) {
		return "", err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o750); err != nil {
		return "", fmt.Errorf("create token directory: %w", err)
	}
	random := make([]byte, 32)
	if _, err := rand.Read(random); err != nil {
		return "", err
	}
	token := base64.RawURLEncoding.EncodeToString(random)
	file, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if errors.Is(err, os.ErrExist) {
		return ReadToken(path)
	}
	if err != nil {
		return "", fmt.Errorf("create worker token: %w", err)
	}
	if _, err := file.WriteString(token + "\n"); err != nil {
		_ = file.Close()
		return "", fmt.Errorf("write worker token: %w", err)
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		return "", err
	}
	if err := file.Close(); err != nil {
		return "", err
	}
	return token, nil
}

func ReadToken(path string) (string, error) {
	value, err := os.ReadFile(strings.TrimSpace(path))
	if err != nil {
		return "", err
	}
	token := strings.TrimSpace(string(value))
	if len(token) < 32 {
		return "", errors.New("worker token file is invalid")
	}
	return token, nil
}

func WaitForToken(path string, timeout time.Duration) (string, error) {
	deadline := time.Now().Add(timeout)
	for {
		token, err := ReadToken(path)
		if err == nil {
			return token, nil
		}
		if !errors.Is(err, os.ErrNotExist) || time.Now().After(deadline) {
			return "", err
		}
		time.Sleep(100 * time.Millisecond)
	}
}
