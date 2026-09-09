package replica

import (
	"bytes"
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"encoding/xml"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"sort"
	"strings"
	"time"
)

// S3 is the smallest client Spin needs: put, get, delete and list, signed
// with SigV4, path-style, against any S3-compatible endpoint. No SDK: the
// server runs on HopOS with the standard library only.
type S3 struct {
	Endpoint  string
	Bucket    string
	Region    string
	AccessKey string
	SecretKey string
	Client    *http.Client
}

var ErrNotFound = errors.New("object not found")

type Object struct {
	Key  string
	Size int64
}

func (c *S3) client() *http.Client {
	if c.Client != nil {
		return c.Client
	}
	return &http.Client{Timeout: 5 * time.Minute}
}

func (c *S3) region() string {
	if strings.TrimSpace(c.Region) == "" {
		return "auto"
	}
	return c.Region
}

// Put stores body under key; the payload hash travels in the request so a
// corrupted upload is refused by the store.
func (c *S3) Put(ctx context.Context, key string, body []byte) error {
	response, err := c.do(ctx, http.MethodPut, key, nil, body)
	if err != nil {
		return err
	}
	defer response.Body.Close()
	return statusError("put "+key, response)
}

func (c *S3) Get(ctx context.Context, key string) ([]byte, error) {
	response, err := c.do(ctx, http.MethodGet, key, nil, nil)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	if response.StatusCode == http.StatusNotFound {
		return nil, ErrNotFound
	}
	if err := statusError("get "+key, response); err != nil {
		return nil, err
	}
	return io.ReadAll(response.Body)
}

func (c *S3) Delete(ctx context.Context, key string) error {
	response, err := c.do(ctx, http.MethodDelete, key, nil, nil)
	if err != nil {
		return err
	}
	defer response.Body.Close()
	if response.StatusCode == http.StatusNotFound {
		return nil
	}
	return statusError("delete "+key, response)
}

// List returns every object under prefix, in key order.
func (c *S3) List(ctx context.Context, prefix string) ([]Object, error) {
	var objects []Object
	token := ""
	for {
		query := url.Values{"list-type": {"2"}, "prefix": {prefix}, "max-keys": {"1000"}}
		if token != "" {
			query.Set("continuation-token", token)
		}
		response, err := c.do(ctx, http.MethodGet, "", query, nil)
		if err != nil {
			return nil, err
		}
		if err := statusError("list "+prefix, response); err != nil {
			response.Body.Close()
			return nil, err
		}
		var listing struct {
			Contents []struct {
				Key  string `xml:"Key"`
				Size int64  `xml:"Size"`
			} `xml:"Contents"`
			IsTruncated           bool   `xml:"IsTruncated"`
			NextContinuationToken string `xml:"NextContinuationToken"`
		}
		err = xml.NewDecoder(response.Body).Decode(&listing)
		response.Body.Close()
		if err != nil {
			return nil, fmt.Errorf("decode listing: %w", err)
		}
		for _, item := range listing.Contents {
			objects = append(objects, Object{Key: item.Key, Size: item.Size})
		}
		if !listing.IsTruncated || listing.NextContinuationToken == "" {
			break
		}
		token = listing.NextContinuationToken
	}
	sort.Slice(objects, func(i, j int) bool { return objects[i].Key < objects[j].Key })
	return objects, nil
}

func statusError(what string, response *http.Response) error {
	if response.StatusCode >= 200 && response.StatusCode < 300 {
		return nil
	}
	body, _ := io.ReadAll(io.LimitReader(response.Body, 2048))
	return fmt.Errorf("s3 %s: status %d: %s", what, response.StatusCode, strings.TrimSpace(string(body)))
}

func (c *S3) do(ctx context.Context, method, key string, query url.Values, body []byte) (*http.Response, error) {
	endpoint, err := url.Parse(strings.TrimRight(strings.TrimSpace(c.Endpoint), "/"))
	if err != nil || endpoint.Host == "" {
		return nil, fmt.Errorf("invalid S3 endpoint %q", c.Endpoint)
	}
	canonicalPath := strings.TrimRight(endpoint.Path, "/") + "/" + escapePath(c.Bucket)
	if key != "" {
		canonicalPath += "/" + escapePath(key)
	}
	canonicalQuery := ""
	if len(query) > 0 {
		canonicalQuery = query.Encode()
	}
	target := endpoint.Scheme + "://" + endpoint.Host + canonicalPath
	if canonicalQuery != "" {
		target += "?" + canonicalQuery
	}
	request, err := http.NewRequestWithContext(ctx, method, target, bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	request.ContentLength = int64(len(body))
	now := time.Now().UTC()
	payloadHash := sha256hex(body)
	request.Header.Set("x-amz-content-sha256", payloadHash)
	request.Header.Set("x-amz-date", now.Format("20060102T150405Z"))
	if method == http.MethodPut {
		request.Header.Set("Content-Type", "application/octet-stream")
	}
	signed := []string{"host", "x-amz-content-sha256", "x-amz-date"}
	canonicalHeaders := "host:" + endpoint.Host + "\n" +
		"x-amz-content-sha256:" + payloadHash + "\n" +
		"x-amz-date:" + request.Header.Get("x-amz-date") + "\n"
	canonicalRequest := strings.Join([]string{method, canonicalPath, canonicalQuery, canonicalHeaders, strings.Join(signed, ";"), payloadHash}, "\n")
	scope := now.Format("20060102") + "/" + c.region() + "/s3/aws4_request"
	stringToSign := strings.Join([]string{"AWS4-HMAC-SHA256", request.Header.Get("x-amz-date"), scope, sha256hex([]byte(canonicalRequest))}, "\n")
	key1 := hmacSHA256([]byte("AWS4"+c.SecretKey), now.Format("20060102"))
	key2 := hmacSHA256(key1, c.region())
	key3 := hmacSHA256(key2, "s3")
	signingKey := hmacSHA256(key3, "aws4_request")
	signature := hex.EncodeToString(hmacSHA256(signingKey, stringToSign))
	request.Header.Set("Authorization", "AWS4-HMAC-SHA256 Credential="+c.AccessKey+"/"+scope+", SignedHeaders="+strings.Join(signed, ";")+", Signature="+signature)
	return c.client().Do(request)
}

func sha256hex(data []byte) string {
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:])
}

func hmacSHA256(key []byte, data string) []byte {
	mac := hmac.New(sha256.New, key)
	mac.Write([]byte(data))
	return mac.Sum(nil)
}

// escapePath encodes a key the way SigV4 canonicalises it: every segment
// percent-encoded except unreserved characters, slashes kept.
func escapePath(value string) string {
	segments := strings.Split(value, "/")
	for index, segment := range segments {
		segments[index] = escapeSegment(segment)
	}
	return strings.Join(segments, "/")
}

func escapeSegment(segment string) string {
	var out strings.Builder
	for _, char := range []byte(segment) {
		switch {
		case char >= 'A' && char <= 'Z', char >= 'a' && char <= 'z', char >= '0' && char <= '9', char == '-', char == '_', char == '.', char == '~':
			out.WriteByte(char)
		default:
			fmt.Fprintf(&out, "%%%02X", char)
		}
	}
	return out.String()
}
