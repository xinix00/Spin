package replica

import (
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"net/http"
	"strings"
	"testing"
)

type roundTripFunc func(*http.Request) (*http.Response, error)

func (f roundTripFunc) RoundTrip(r *http.Request) (*http.Response, error) { return f(r) }
func response(code int, body string) *http.Response {
	return &http.Response{StatusCode: code, Body: io.NopCloser(strings.NewReader(body)), Header: http.Header{}}
}

func TestS3CanonicalSignatureAndPagination(t *testing.T) {
	calls := 0
	client := &S3{Endpoint: "https://s3.example.test", Bucket: "bucket", AccessKey: "access", SecretKey: "secret", Region: "auto"}
	client.Client = &http.Client{Transport: roundTripFunc(func(req *http.Request) (*http.Response, error) {
		calls++
		wantQuery := "list-type=2&max-keys=1000&prefix=a%20b%2F"
		if calls == 2 {
			wantQuery = "continuation-token=a%2Bb%20%2F&" + wantQuery
		}
		if req.URL.RawQuery != wantQuery {
			t.Fatalf("query %q, want %q", req.URL.RawQuery, wantQuery)
		}
		stamp := req.Header.Get("x-amz-date")
		empty := sha256.Sum256(nil)
		payload := hex.EncodeToString(empty[:])
		canonical := "GET\n/bucket\n" + wantQuery + "\nhost:s3.example.test\nx-amz-content-sha256:" + payload + "\nx-amz-date:" + stamp + "\n\nhost;x-amz-content-sha256;x-amz-date\n" + payload
		sum := sha256.Sum256([]byte(canonical))
		scope := stamp[:8] + "/auto/s3/aws4_request"
		sign := func(key []byte, value string) []byte {
			mac := hmac.New(sha256.New, key)
			mac.Write([]byte(value))
			return mac.Sum(nil)
		}
		key := []byte("AWS4secret")
		for _, value := range []string{stamp[:8], "auto", "s3", "aws4_request"} {
			key = sign(key, value)
		}
		signature := hex.EncodeToString(sign(key, "AWS4-HMAC-SHA256\n"+stamp+"\n"+scope+"\n"+hex.EncodeToString(sum[:])))
		want := "AWS4-HMAC-SHA256 Credential=access/" + scope + ", SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=" + signature
		if req.Header.Get("Authorization") != want {
			t.Fatal("signature differs from canonical request")
		}
		if calls == 1 {
			return response(200, `<ListBucketResult><Contents><Key>a b/z</Key><Size>4</Size></Contents><IsTruncated>true</IsTruncated><NextContinuationToken>a+b /</NextContinuationToken></ListBucketResult>`), nil
		}
		return response(200, `<ListBucketResult><Contents><Key>a b/a</Key><Size>2</Size></Contents></ListBucketResult>`), nil
	})}
	objects, err := client.List(context.Background(), "a b/")
	if err != nil {
		t.Fatal(err)
	}
	if calls != 2 || len(objects) != 2 || objects[0].Key != "a b/a" {
		t.Fatalf("listing %+v, calls %d", objects, calls)
	}
}

func TestS3RejectsIncompletePagination(t *testing.T) {
	for _, token := range []string{"", "same"} {
		t.Run(fmt.Sprintf("token=%s", token), func(t *testing.T) {
			client := &S3{Endpoint: "https://s3.example.test", Bucket: "bucket", Client: &http.Client{Transport: roundTripFunc(func(*http.Request) (*http.Response, error) {
				return response(200, "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>"+token+"</NextContinuationToken></ListBucketResult>"), nil
			})}}
			if _, err := client.List(context.Background(), "prefix"); err == nil {
				t.Fatal("accepted truncated listing")
			}
		})
	}
}

func TestS3EscapesKeysAndReportsErrors(t *testing.T) {
	client := &S3{Endpoint: "https://s3.example.test/proxy%20path", Bucket: "bucket"}
	client.Client = &http.Client{Transport: roundTripFunc(func(req *http.Request) (*http.Response, error) {
		if req.URL.EscapedPath() != "/proxy%20path/bucket/a%20b/%C3%A9%2B" {
			t.Fatalf("path %s", req.URL.EscapedPath())
		}
		switch req.Method {
		case "PUT":
			return response(503, "offline"), nil
		case "GET":
			return response(404, ""), nil
		default:
			return response(204, ""), nil
		}
	})}
	if err := client.Put(context.Background(), "a b/é+", []byte("body")); err == nil || !strings.Contains(err.Error(), "503") {
		t.Fatalf("put error %v", err)
	}
	if _, err := client.Get(context.Background(), "a b/é+"); err != ErrNotFound {
		t.Fatalf("get error %v", err)
	}
	if err := client.Delete(context.Background(), "a b/é+"); err != nil {
		t.Fatal(err)
	}
}
