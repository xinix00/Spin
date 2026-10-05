"""Local, bounded S3 fixture: verify actual SigV4 and fragment chunked responses."""
import hashlib
import hmac
import http.server
import threading
import time
import urllib.parse
from xml.sax.saxutils import escape

ACCESS = 'spin-native-fixture'
SECRET = 'spin-native-fixture-secret'
BUCKET = 'native-test'
MASTER = 'AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8='


class Bucket(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(('127.0.0.1', 0), Handler)
        self.objects = {}
        self.errors = []
        self.verified = 0
        self.connections = 0
        self.reused = 0
        self.pause_next = False
        self.drop_next_segment_get = False
        # The dropped segment key and how often it was fetched again afterwards.
        self.dropped_segment = None
        self.refetched_segment = 0
        self.blocked = threading.Event()
        self.release = threading.Event()
        self.lock = threading.Lock()
        threading.Thread(target=self.serve_forever, daemon=True).start()

    def environment(self):
        return {
            'SPIN_MASTER_KEY': MASTER,
            'SPIN_S3_ENDPOINT': f'http://10.0.2.2:{self.server_port}',
            'SPIN_S3_BUCKET': BUCKET,
            'SPIN_S3_ACCESS_KEY': ACCESS,
            'SPIN_S3_SECRET_KEY': SECRET,
        }

    def close(self):
        self.shutdown()
        self.server_close()


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def setup(self):
        super().setup()
        self.requests_served = 0
        with self.server.lock:
            self.server.connections += 1

    def log_message(self, *_):
        pass

    def reply(self, status, body=b''):
        self.send_response(status)
        self.send_header('Connection', 'keep-alive')
        if body:
            self.send_header('Transfer-Encoding', 'chunked')
            self.end_headers()
            for offset in range(0, len(body), 8192):
                chunk = body[offset:offset+8192]
                self.wfile.write(f'{len(chunk):x}\r\n'.encode())
                self.wfile.flush()
                # Force Pending between the chunk header, body and trailing CRLF.
                time.sleep(.003)
                self.wfile.write(chunk)
                self.wfile.flush()
                time.sleep(.003)
                self.wfile.write(b'\r\n')
            self.wfile.write(b'0\r\n\r\n')
        else:
            self.send_header('Content-Length', '0')
            self.end_headers()
        self.close_connection = False

    def dispatch(self):
        try:
            self.perform()
        except (BrokenPipeError, ConnectionResetError):
            pass  # The test deliberately kills the VM without graceful shutdown.
        except Exception as error:
            with self.server.lock:
                self.server.errors.append(str(error))
            self.reply(403)

    def perform(self):
        length = int(self.headers.get('Content-Length', '0'))
        assert 0 <= length <= 32 << 20, 'object exceeds fixture budget'
        body = self.rfile.read(length)
        assert len(body) == length, 'truncated request'
        scheme, raw = self.headers.get('Authorization', '').split(' ', 1)
        assert scheme == 'AWS4-HMAC-SHA256', 'missing SigV4'
        fields = dict(part.strip().split('=', 1) for part in raw.split(','))
        access, scope = fields['Credential'].split('/', 1)
        assert access == ACCESS, 'wrong access key'
        date, region, service, terminal = scope.split('/')
        assert (region, service, terminal) == ('us-east-1', 's3', 'aws4_request')
        digest = hashlib.sha256(body).hexdigest()
        assert self.headers['X-Amz-Content-Sha256'] == digest, 'payload hash mismatch'
        signed = fields['SignedHeaders']
        headers = ''.join(f'{name}:{" ".join(self.headers[name].split())}\n'
                          for name in signed.split(';'))
        url = urllib.parse.urlsplit(self.path)
        quote = lambda value: urllib.parse.quote(value, safe='-_.~')
        query = '&'.join(f'{key}={value}' for key, value in sorted(
            (quote(k), quote(v)) for k, v in urllib.parse.parse_qsl(url.query, keep_blank_values=True)))
        canonical = '\n'.join((self.command, url.path, query, headers, signed, digest))
        signing = '\n'.join((scheme, self.headers['X-Amz-Date'], scope,
                             hashlib.sha256(canonical.encode()).hexdigest()))
        key = ('AWS4' + SECRET).encode()
        for part in (date, region, service, terminal):
            key = hmac.digest(key, part.encode(), 'sha256')
        assert hmac.compare_digest(hmac.new(key, signing.encode(), 'sha256').hexdigest(),
                                   fields['Signature']), 'signature mismatch'
        path = urllib.parse.unquote(url.path)
        assert path == f'/{BUCKET}' or path.startswith(f'/{BUCKET}/'), 'wrong bucket'
        key = path[len(BUCKET)+2:]
        with self.server.lock:
            pause = self.server.pause_next
            self.server.pause_next = False
        if pause:
            self.server.blocked.set()
            self.server.release.wait(timeout=12)
        with self.server.lock:
            self.server.verified += 1
            if self.requests_served:
                self.server.reused += 1
            self.requests_served += 1
            if self.command == 'GET' and key == self.server.dropped_segment:
                self.server.refetched_segment += 1
            if self.command == 'GET' and '/data/' in key and self.server.drop_next_segment_get:
                self.server.drop_next_segment_get = False
                self.server.dropped_segment = key
                self.send_response(200)
                self.send_header("Content-Length", "16")
                self.end_headers()
                self.wfile.write(b"partial")
                self.wfile.flush()
                self.close_connection = True
                return
            if self.command == 'GET' and not key:
                query = urllib.parse.parse_qs(url.query)
                prefix = query.get('prefix', [''])[0]
                maximum = int(query.get('max-keys', ['1000'])[0])
                found = sorted(k for k in self.server.objects if k.startswith(prefix))
                if query.get('delimiter') == ['/']:
                    found = sorted({prefix+k[len(prefix):].split('/')[0]+'/' for k in found if '/' in k[len(prefix):]})
                    entries = ''.join(f'<CommonPrefixes><Prefix>{escape(k)}</Prefix></CommonPrefixes>' for k in found[:maximum])
                else:
                    entries = ''.join(f'<Contents><Key>{escape(k)}</Key><Size>{len(self.server.objects[k])}</Size></Contents>' for k in found[:maximum])
                truncated = str(len(found) > maximum).lower()
                status, response = 200, (f'<ListBucketResult><IsTruncated>{truncated}</IsTruncated>{entries}</ListBucketResult>').encode()
            elif self.command == 'GET':
                status, response = (200, self.server.objects[key]) if key in self.server.objects else (404, b'')
            elif self.command == 'PUT':
                assert sum(map(len, self.server.objects.values())) + len(body) <= 64 << 20, 'bucket exceeds fixture budget'
                self.server.objects[key] = body
                status, response = 200, b''
            elif self.command == 'DELETE':
                self.server.objects.pop(key, None)
                status, response = 204, b''
            else:
                raise AssertionError('unexpected method')
        self.reply(status, response)

    do_GET = do_PUT = do_DELETE = dispatch
