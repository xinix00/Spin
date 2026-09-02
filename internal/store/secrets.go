package store

import (
	"crypto/aes"
	"crypto/cipher"
	"crypto/rand"
	"encoding/base64"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"easyacp/internal/domain"
)

const encryptedValuePrefix = "enc:v1:"

type OpenOptions struct {
	// MasterKey is a base64-encoded 32-byte AES key. MasterKeyFile is used when
	// it is empty; a missing key file is created atomically with mode 0600.
	MasterKey     string
	MasterKeyFile string
}

type secretCipher struct {
	aead cipher.AEAD
}

func newSecretCipher(options OpenOptions, statePath string) (*secretCipher, error) {
	var key []byte
	var err error
	if encoded := strings.TrimSpace(options.MasterKey); encoded != "" {
		key, err = base64.RawStdEncoding.DecodeString(encoded)
		if err != nil {
			key, err = base64.StdEncoding.DecodeString(encoded)
		}
		if err != nil {
			return nil, fmt.Errorf("decode master key: %w", err)
		}
	} else {
		keyFile := strings.TrimSpace(options.MasterKeyFile)
		if keyFile == "" && statePath != "" {
			keyFile = statePath + ".key"
		}
		if keyFile == "" {
			key = make([]byte, 32)
			if _, err := io.ReadFull(rand.Reader, key); err != nil {
				return nil, err
			}
		} else {
			key, err = loadOrCreateMasterKey(keyFile)
			if err != nil {
				return nil, err
			}
		}
	}
	if len(key) != 32 {
		return nil, fmt.Errorf("master key must decode to exactly 32 bytes; got %d", len(key))
	}
	block, err := aes.NewCipher(key)
	if err != nil {
		return nil, err
	}
	aead, err := cipher.NewGCM(block)
	if err != nil {
		return nil, err
	}
	return &secretCipher{aead: aead}, nil
}

func loadOrCreateMasterKey(path string) ([]byte, error) {
	encoded, err := os.ReadFile(path)
	if err == nil {
		key, decodeErr := base64.RawStdEncoding.DecodeString(strings.TrimSpace(string(encoded)))
		if decodeErr != nil || len(key) != 32 {
			return nil, fmt.Errorf("read master key %s: invalid base64-encoded 32-byte key", path)
		}
		return key, nil
	}
	if !errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf("read master key %s: %w", path, err)
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return nil, fmt.Errorf("create master key directory: %w", err)
	}
	key := make([]byte, 32)
	if _, err := io.ReadFull(rand.Reader, key); err != nil {
		return nil, err
	}
	file, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if errors.Is(err, os.ErrExist) {
		return loadOrCreateMasterKey(path)
	}
	if err != nil {
		return nil, fmt.Errorf("create master key %s: %w", path, err)
	}
	encodedKey := base64.RawStdEncoding.EncodeToString(key) + "\n"
	if _, err := file.WriteString(encodedKey); err != nil {
		_ = file.Close()
		return nil, fmt.Errorf("write master key: %w", err)
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		return nil, fmt.Errorf("sync master key: %w", err)
	}
	if err := file.Close(); err != nil {
		return nil, fmt.Errorf("close master key: %w", err)
	}
	return key, nil
}

func (c *secretCipher) encrypt(value, purpose string) (string, error) {
	if value == "" {
		return "", nil
	}
	nonce := make([]byte, c.aead.NonceSize())
	if _, err := io.ReadFull(rand.Reader, nonce); err != nil {
		return "", err
	}
	sealed := c.aead.Seal(nil, nonce, []byte(value), []byte(purpose))
	payload := append(nonce, sealed...)
	return encryptedValuePrefix + base64.RawStdEncoding.EncodeToString(payload), nil
}

func (c *secretCipher) decrypt(value, purpose string) (string, error) {
	if value == "" || !strings.HasPrefix(value, encryptedValuePrefix) {
		return value, nil // plaintext from the legacy schema; next save migrates it
	}
	payload, err := base64.RawStdEncoding.DecodeString(strings.TrimPrefix(value, encryptedValuePrefix))
	if err != nil || len(payload) < c.aead.NonceSize() {
		return "", errors.New("invalid encrypted secret payload")
	}
	nonce, sealed := payload[:c.aead.NonceSize()], payload[c.aead.NonceSize():]
	plaintext, err := c.aead.Open(nil, nonce, sealed, []byte(purpose))
	if err != nil {
		return "", errors.New("cannot decrypt secret; the master key is missing or incorrect")
	}
	return string(plaintext), nil
}

func (s *Store) encryptedStateLocked() (persistedState, error) {
	out := s.state
	out.MCPServers = make(map[string]domain.MCPServer, len(s.state.MCPServers))
	for id, server := range s.state.MCPServers {
		server.Env = append([]domain.MCPSecret{}, server.Env...)
		server.Headers = append([]domain.MCPSecret{}, server.Headers...)
		for index := range server.Env {
			value, err := s.secrets.encrypt(server.Env[index].Value, "mcp:"+id+":env:"+server.Env[index].Name)
			if err != nil {
				return persistedState{}, err
			}
			server.Env[index].Value = value
		}
		for index := range server.Headers {
			value, err := s.secrets.encrypt(server.Headers[index].Value, "mcp:"+id+":header:"+server.Headers[index].Name)
			if err != nil {
				return persistedState{}, err
			}
			server.Headers[index].Value = value
		}
		out.MCPServers[id] = server
	}
	out.GitAccounts = make(map[string]domain.GitAccount, len(s.state.GitAccounts))
	for id, account := range s.state.GitAccounts {
		var err error
		account.AccessToken, err = s.secrets.encrypt(account.AccessToken, "git-account:"+id+":access")
		if err != nil {
			return persistedState{}, err
		}
		account.RefreshToken, err = s.secrets.encrypt(account.RefreshToken, "git-account:"+id+":refresh")
		if err != nil {
			return persistedState{}, err
		}
		out.GitAccounts[id] = account
	}
	out.GitOAuthConfigurations = make(map[string]domain.GitOAuthConfiguration, len(s.state.GitOAuthConfigurations))
	for provider, configuration := range s.state.GitOAuthConfigurations {
		var err error
		configuration.ClientSecret, err = s.secrets.encrypt(configuration.ClientSecret, "git-oauth:"+provider+":client-secret")
		if err != nil {
			return persistedState{}, err
		}
		out.GitOAuthConfigurations[provider] = configuration
	}
	return out, nil
}

func (s *Store) decryptSecretsLocked() error {
	for id, server := range s.state.MCPServers {
		for index := range server.Env {
			value, err := s.secrets.decrypt(server.Env[index].Value, "mcp:"+id+":env:"+server.Env[index].Name)
			if err != nil {
				return fmt.Errorf("MCP server %s env %s: %w", id, server.Env[index].Name, err)
			}
			server.Env[index].Value = value
		}
		for index := range server.Headers {
			value, err := s.secrets.decrypt(server.Headers[index].Value, "mcp:"+id+":header:"+server.Headers[index].Name)
			if err != nil {
				return fmt.Errorf("MCP server %s header %s: %w", id, server.Headers[index].Name, err)
			}
			server.Headers[index].Value = value
		}
		s.state.MCPServers[id] = server
	}
	for id, account := range s.state.GitAccounts {
		var err error
		account.AccessToken, err = s.secrets.decrypt(account.AccessToken, "git-account:"+id+":access")
		if err != nil {
			return fmt.Errorf("Git account %s access token: %w", id, err)
		}
		account.RefreshToken, err = s.secrets.decrypt(account.RefreshToken, "git-account:"+id+":refresh")
		if err != nil {
			return fmt.Errorf("Git account %s refresh token: %w", id, err)
		}
		s.state.GitAccounts[id] = account
	}
	for provider, configuration := range s.state.GitOAuthConfigurations {
		value, err := s.secrets.decrypt(configuration.ClientSecret, "git-oauth:"+provider+":client-secret")
		if err != nil {
			return fmt.Errorf("Git OAuth provider %s client secret: %w", provider, err)
		}
		configuration.ClientSecret = value
		s.state.GitOAuthConfigurations[provider] = configuration
	}
	return nil
}
