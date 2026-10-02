#!/usr/bin/env python3
"""Native HopOS HTTP, duurzame login en harde herstart op een tijdelijk volume."""
import functools
import concurrent.futures
import argparse
import http.server
import io
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
import urllib.request
import zipfile
from s3_fixture import Bucket
from ntp_fixture import Clock
from runner_fixture import exercise as exercise_runner
from tenancy_fixture import exercise as exercise_tenants

ROOT = Path(__file__).resolve().parents[2]
TARGET = 'aarch64-unknown-none-softfloat'
OUT = ROOT / 'target/port-native'

class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_):
        pass

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--s3', action='store_true', help='verify SigV4 and restore on an empty disk')
    parser.add_argument('--runner', action='store_true', help='exercise the macOS release runner against real, isolated Docker containers')
    parser.add_argument('--tenants', action='store_true', help='verify two isolated native domains and their recovery')
    options = parser.parse_args()
    if options.tenants and options.runner:
        parser.error('--tenants and --runner are separate fixture runs')
    OUT.mkdir(parents=True, exist_ok=True)
    sdk = Path(os.environ['HOPOS_SDK'])
    binary = ROOT / 'target' / TARGET / 'release/spin-hopos-server'
    with binary.open('rb') as stream:
        binary_sha256 = hashlib.file_digest(stream, 'sha256').hexdigest()
    runner_sha256 = None
    if options.runner:
        with (ROOT / 'target/release/spin-client').open('rb') as stream:
            runner_sha256 = hashlib.file_digest(stream, 'sha256').hexdigest()
    env = dict(os.environ, APP='hop', CARGO_INCREMENTAL='0')
    env.setdefault('HOP_DIR', str(ROOT.parent / 'hop/hop'))
    for key in ('ROLE', 'APPENV', 'ARTIFACT', 'GUI'):
        env.pop(key, None)
    held = []
    for name in ('SYSPORT', 'AGENTPORT', 'LEADERPORT', 'WEBPORT'):
        sock = socket.socket()
        sock.bind(('127.0.0.1', 0))
        env[name] = str(sock.getsockname()[1])
        held.append(sock)
    with tempfile.TemporaryDirectory(prefix='spin-hopos-') as tmp:
        tmp = Path(tmp)
        env['DISK'] = str(tmp / 'disk.img')
        env['DISK_MIB'] = '256'
        subprocess.run(['/opt/homebrew/opt/llvm/bin/llvm-objcopy', '--strip-debug', str(binary), str(tmp/'spin.elf')], check=True)
        artifacts = http.server.ThreadingHTTPServer(('127.0.0.1',0), functools.partial(Quiet,directory=str(tmp)))
        artifacts.daemon_threads = True
        threading.Thread(target=artifacts.serve_forever,daemon=True).start()
        job = {'name':'spin-port-test','driver':'hop','count':1,'update_policy':'recreate', 'artifacts':[{'url':f'http://10.0.2.2:{artifacts.server_port}/spin.elf'}],
               'memory_limit':536870912, 'ports':{'http':80}, 'volumes':{'/volumes/spin-port-test':'/data'},
               'env':{'SPIN_REPLICATION':'off','SPIN_DATABASE':'spin.sqlite','SPIN_PORT':'80','SPIN_DATA_DIR':'/data','SPIN_INTERNAL_URL':'http://spin.internal'}}
        if options.tenants:
            job['env'].pop('SPIN_DATABASE')
            # Empty allowlist: lazy opening, then discovery from disk and S3 catalogs.
        bucket = Bucket() if options.s3 else None
        if bucket:
            job['env'].pop('SPIN_REPLICATION')
            job['env'].update(bucket.environment())
        for sock in held:
            sock.close()
        url = f"http://127.0.0.1:{env['WEBPORT']}"
        host_headers = {'Host':'alpha.spin.test'} if options.tenants else {}
        origin = 'http://alpha.spin.test' if options.tenants else url
        def call(path, body=None, cookie=None, extra_headers=None):
            data = None if body is None else json.dumps(body).encode()
            headers = dict(host_headers, **{'Content-Type':'application/json', 'Origin':origin})
            if cookie: headers['Cookie'] = cookie
            if extra_headers: headers.update(extra_headers)
            with urllib.request.urlopen(urllib.request.Request(url+path,data=data,headers=headers),timeout=45) as response:
                return response.status, dict(response.headers), response.read()
        def runner_call(path, token, method='GET', data=None, headers=None):
            fields = dict(host_headers, Authorization='Bearer '+token)
            if headers: fields.update(headers)
            if isinstance(data, dict):
                data = json.dumps(data).encode()
                fields['Content-Type'] = 'application/json'
            with urllib.request.urlopen(urllib.request.Request(url+path, data=data, headers=fields, method=method), timeout=60) as response:
                return response.status, dict(response.headers), response.read()
        snapshot_bytes = bytes(range(251)) * 8400  # More than two SQLite upload chunks.
        snapshot_digest = 'sha256:' + hashlib.sha256(snapshot_bytes).hexdigest()
        clock = Clock()
        env['BOOTARGS'] = f'hopos.ntp=10.0.2.2:{clock.port}'
        try:
            cookie = None
            for boot in range(3 if bucket else 2):
                if boot == 2:
                    # Only this harness's disposable VM disk; retain the independent bucket.
                    Path(env['DISK']).unlink()
                    bucket.drop_next_segment_get = True
                log = OUT / f'{"s3-" if bucket else ""}boot-{boot}.log'
                with log.open('w') as stream:
                    process = subprocess.Popen(['sh',str(sdk/'image/qemu-run.sh')],cwd=sdk,env=env,stdin=subprocess.DEVNULL,stdout=stream,stderr=subprocess.STDOUT,start_new_session=True)
                    try:
                        posted = False
                        deadline = time.monotonic()+300
                        while time.monotonic()<deadline:
                            text = log.read_text(errors='replace')
                            if any(x in text for x in ('SPIN_BOOT_FAILED','HOPOS_PANIC','HOPOS_EXCEPTION','HOPOS_APP_PANIC')):
                                raise RuntimeError(f'native boot failed; see {log}')
                            if boot in (0, 2) and not posted and 'HOP_LEADER' in text and 'HOP_UP' in text:
                                req = urllib.request.Request(f"http://127.0.0.1:{env['LEADERPORT']}/v1/jobs",data=json.dumps(job).encode(),headers={'Content-Type':'application/json'},method='POST')
                                with urllib.request.urlopen(req,timeout=15) as response:
                                    assert response.status in (200,201,202)
                                posted = True
                            if 'SPIN_LISTEN' in text:
                                break
                            if process.poll() is not None:
                                raise RuntimeError(f'QEMU stopped; see {log}')
                            time.sleep(.1)
                        else:
                            raise RuntimeError(f'native listener did not start; see {log}')
                        deadline = time.monotonic() + 120
                        while True:
                            try:
                                assert call('/healthz')[0] == 200
                                break
                            except urllib.error.HTTPError as error:
                                if error.code != 503 or time.monotonic() >= deadline:
                                    raise
                                time.sleep(.1)
                        status = json.loads(call('/api/auth/status')[2])
                        if boot == 0:
                            assert not status['configured']
                            code, headers, body = call('/api/auth/setup', {'username':'native-test','password':'native-test-password-42','display_name':'Native Test'})
                            assert code == 201, body
                            if options.tenants:
                                assert headers['Set-Cookie'].startswith('__Host-spin_session=') and '; Secure' in headers['Set-Cookie']
                            cookie = headers['Set-Cookie'].split(';')[0]
                        else:
                            assert status['configured']
                        code, headers, body = call('/api/state', cookie=cookie)
                        assert code == 200, body
                        assert json.loads(body)['current_user']['username'] == 'native-test'
                        assert 'no-store' in headers.get('Cache-Control','')
                        _, auth_headers, auth_body = call('/api/auth/status', cookie=cookie)
                        if 'Set-Cookie' in auth_headers:
                            cookie = auth_headers['Set-Cookie'].split(';')[0]
                        auth = json.loads(auth_body)
                        token = json.loads(call('/api/runners/token', cookie=cookie)[2])['token']
                        if boot == 0 and options.runner:
                            exercise_runner(ROOT, tmp, url, cookie, auth['csrf_token'], token, call)
                        if boot == 0:
                            _, _, payload = runner_call('/api/uploads', token, 'POST', {'kind':'snapshot', 'size':len(snapshot_bytes), 'snapshot':{'digest':snapshot_digest}})
                            upload_id = json.loads(payload)['id']
                            for chunk in (1, 0, 2):
                                offset = chunk * (1 << 20)
                                code, _, _ = runner_call('/api/uploads/'+upload_id, token, 'PUT', snapshot_bytes[offset:offset+(1 << 20)], {'X-Spin-Upload-Offset':str(offset)})
                                assert code == 200
                            code, _, payload = runner_call('/api/uploads/'+upload_id+'/complete', token, 'POST', {})
                            assert code == 200, payload
                            assert json.loads(payload)['digest'] == snapshot_digest
                        fetched = bytearray()
                        for offset in range(0, len(snapshot_bytes), 1 << 20):
                            code, _, payload = runner_call('/api/snapshots/'+snapshot_digest+'?offset='+str(offset), token)
                            assert code == 200
                            fetched.extend(payload)
                        assert fetched == snapshot_bytes, 'archive changed across SQLite epochs or recovery'
                        code, _, payload = call('/api/backup-ticket', {}, cookie, {'X-Spin-CSRF':auth['csrf_token']})
                        assert code == 201, payload
                        download = json.loads(payload)['url']
                        code, headers, payload = call(download, cookie=cookie)
                        assert code == 200
                        assert int(headers['Content-Length']) == len(payload)
                        assert headers['X-Spin-Backup-Contains-Secrets'] == 'true'
                        backup_bytes = payload
                        with zipfile.ZipFile(io.BytesIO(payload)) as archive:
                            assert set(archive.namelist()) == {'spin.db', 'master-key.txt'}
                            assert archive.testzip() is None
                            assert len(archive.read('master-key.txt').strip()) in (43,44)
                            extracted = tmp / f'backup-{boot}.db'
                            extracted.write_bytes(archive.read('spin.db'))
                        with sqlite3.connect(f'file:{extracted}?mode=ro', uri=True) as connection:
                            assert connection.execute('PRAGMA quick_check').fetchone() == ('ok',)
                            assert connection.execute("SELECT COUNT(*) FROM spin_kv WHERE key='state'").fetchone() == (1,)
                        assert call('/api/state', cookie=cookie)[0] == 200
                        try:
                            call(download, cookie=cookie)
                            raise AssertionError('backup ticket accepted twice')
                        except urllib.error.HTTPError as error:
                            assert error.code == 403
                        if boot == 0:
                            # A bad backup must preserve the token changed after the backup;
                            # a valid restore must replace it and invalidate browser sessions.
                            code, _, rotated = call('/api/runners/token', {}, cookie, {'X-Spin-CSRF':auth['csrf_token']})
                            rotated_token = json.loads(rotated)['token']
                            assert rotated_token != token
                            damaged = bytearray(backup_bytes)
                            damaged[1100] ^= 1
                            for damaged_case, contents in ((True, damaged), (False, backup_bytes)):
                                code, _, upload = call('/api/uploads', {'kind':'restore', 'name':'native-backup.zip', 'size':len(contents)}, cookie, {'X-Spin-CSRF':auth['csrf_token']})
                                assert code == 201
                                restore_id = json.loads(upload)['id']
                                for offset in range(0, len(contents), 1 << 20):
                                    fields = {'Cookie':cookie, 'Origin':origin, 'X-Spin-CSRF':auth['csrf_token'], 'X-Spin-Upload-Offset':str(offset)}
                                    fields.update(host_headers)
                                    request = urllib.request.Request(url+'/api/uploads/'+restore_id, data=bytes(contents[offset:offset+(1 << 20)]), headers=fields, method='PUT')
                                    with urllib.request.urlopen(request, timeout=60) as response:
                                        assert response.status == 200
                                code, _, completion = call('/api/uploads/'+restore_id+'/complete', {}, cookie, {'X-Spin-CSRF':auth['csrf_token']})
                                assert code == 202, completion
                                deadline = time.monotonic() + 90
                                while time.monotonic() < deadline:
                                    restored = json.loads(call('/api/restores/'+restore_id)[2])
                                    if restored['status'] != 'running':
                                        break
                                    time.sleep(.1)
                                else:
                                    raise AssertionError('restore did not finish')
                                assert restored['status'] == ('error' if damaged_case else 'complete'), restored
                                if damaged_case:
                                    assert json.loads(call('/api/runners/token', cookie=cookie)[2])['token'] == rotated_token
                                else:
                                    try:
                                        call('/api/state', cookie=cookie)
                                        raise AssertionError('restored database retained old browser session')
                                    except urllib.error.HTTPError as error:
                                        assert error.code == 401
                                    code, headers, body = call('/api/auth/login', {'username':'native-test','password':'native-test-password-42'})
                                    assert code == 200, body
                                    cookie = headers['Set-Cookie'].split(';')[0]
                                    assert json.loads(call('/api/runners/token', cookie=cookie)[2])['token'] == token
                                    code, _, restored_blob = runner_call('/api/snapshots/'+snapshot_digest+'?offset=0', token)
                                    assert restored_blob == snapshot_bytes[:1 << 20]
                            print('PASS native restore rejects damaged ZIP; valid ZIP atomically restores state + blobs and invalidates sessions', flush=True)
                            for progress in (False, True):
                                current_auth = json.loads(call('/api/auth/status', cookie=cookie)[2])
                                fields = {'Cookie':cookie, 'Origin':origin, 'X-Spin-CSRF':current_auth['csrf_token'], 'Content-Type':'application/zip'}
                                if progress:
                                    fields['Accept'] = 'application/x-ndjson'
                                fields.update(host_headers)
                                request = urllib.request.Request(url+'/api/restore', data=backup_bytes, headers=fields, method='POST')
                                with urllib.request.urlopen(request, timeout=90) as response:
                                    assert response.status == 200
                                    restored = response.read()
                                if progress:
                                    events = [json.loads(line) for line in restored.splitlines() if line]
                                    assert events[-1]['type'] == 'complete', events[-1]
                                    restored = events[-1]['result']
                                else:
                                    restored = json.loads(restored)
                                assert restored['status'] == 'restored', restored
                                code, headers, body = call('/api/auth/login', {'username':'native-test','password':'native-test-password-42'})
                                assert code == 200, body
                                cookie = headers['Set-Cookie'].split(';')[0]
                                assert json.loads(call('/api/runners/token', cookie=cookie)[2])['token'] == token
                            print('PASS native direct restore above 1 MiB, JSON and streamed NDJSON contracts', flush=True)
                        if bucket:
                            # Include session rotation after a restart, not merely the initial snapshot.
                            settled = time.monotonic() + 20
                            deadline = time.monotonic() + 90
                            while time.monotonic() < deadline:
                                if bucket.errors:
                                    raise AssertionError(bucket.errors)
                                # Wait for the dirty commit that contains the new browser session.
                                with bucket.lock:
                                    committed = any('/L0/' in key and key.endswith('.json') for key in bucket.objects)
                                if time.monotonic() >= settled and committed and 'SPIN_REPLICA_SYNCED' in log.read_text():
                                    break
                                time.sleep(.1)
                            else:
                                raise AssertionError(f'Replica did not publish: {log}')
                            if boot == 2:
                                assert 'SPIN_REPLICA_READY reason=Restored' in log.read_text()
                                assert 'SPIN_S3_READ_RETRY attempt=1' in log.read_text()
                                print('PASS interrupted S3 segment GET retries without restarting restore', flush=True)
                            if boot == 1:
                                points = json.loads(call('/api/replica/points', cookie=cookie)[2])['points']
                                assert points and points[0]['generation'] and points[0]['at']
                                point = points[0]
                                code, _, rotated = call('/api/runners/token', {}, cookie, {'X-Spin-CSRF':auth['csrf_token']})
                                assert json.loads(rotated)['token'] != token
                                bucket.blocked.clear()
                                bucket.release.clear()
                                with bucket.lock:
                                    bucket.pause_next = True
                                with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pending:
                                    restoring = pending.submit(call, '/api/replica/restore', {'generation':point['generation'], 'at':point['at']}, cookie, {'X-Spin-CSRF':auth['csrf_token']})
                                    try:
                                        assert bucket.blocked.wait(5), 'restore never reached S3'
                                        started_health = time.monotonic()
                                        # More than the socket pool size: health-only connections must
                                        # recycle without borrowing the parked SQLite owner.
                                        for _ in range(70):
                                            health_headers = {'Host':'beta.spin.test'} if options.tenants and _ % 2 else host_headers
                                            with urllib.request.urlopen(urllib.request.Request(url+'/healthz', headers=health_headers), timeout=2) as response:
                                                assert response.status == 200
                                                assert response.headers['Cache-Control'].startswith('no-store')
                                                response.read()
                                        assert time.monotonic() - started_health < 8, 'HTTP stalled behind S3'
                                        if options.tenants:
                                            secondary = json.loads(call('/api/auth/status', extra_headers={'Host':'beta.spin.test', 'Origin':'http://beta.spin.test'})[2])
                                            assert secondary['configured'], 'secondary application owner stalled or lost state during primary restore'
                                        print('PASS 70 health requests while native SQLite owner waits on S3', flush=True)
                                    finally:
                                        bucket.release.set()
                                    code, _, payload = restoring.result(timeout=45)
                                assert code == 202, payload
                                restore_id = json.loads(payload)['id']
                                deadline = time.monotonic() + 90
                                while time.monotonic() < deadline:
                                    restored = json.loads(call('/api/restores/'+restore_id)[2])
                                    if restored['status'] != 'running':
                                        break
                                    time.sleep(.1)
                                else:
                                    raise AssertionError('Replica point restore did not finish')
                                assert restored['status'] == 'complete', restored
                                code, headers, body = call('/api/auth/login', {'username':'native-test','password':'native-test-password-42'})
                                assert code == 200, body
                                cookie = headers['Set-Cookie'].split(';')[0]
                                assert json.loads(call('/api/runners/token', cookie=cookie)[2])['token'] == token
                                # Include the post-restore login in the subsequent empty-disk recovery.
                                time.sleep(20)
                                assert not bucket.errors
                                print('PASS native Replica catalog and selected point restore', flush=True)
                        if options.tenants:
                            if boot > 0:
                                assert 'SPIN_TENANT_READY domain=beta.spin.test' in log.read_text(errors='replace'), 'secondary tenant was not discovered before its first request'
                            exercise_tenants(call, boot, cookie, token, snapshot_digest)
                        print(f'PASS native HopOS HTTP + durable browser identity + central archive + SQLite ZIP64 backup, boot={boot}',flush=True)
                    finally:
                        if process.poll() is None:
                            os.killpg(process.pid,signal.SIGKILL)
                        process.wait(timeout=5)
        finally:
            clock.close()
            if bucket:
                bucket.close()
            artifacts.shutdown()
            artifacts.server_close()
    if bucket:
        assert not bucket.errors, bucket.errors
        assert bucket.verified > 0
        assert bucket.reused > 20, "native S3 requests did not reuse connections"
        print(f"PASS native S3 keep-alive: {bucket.reused}/{bucket.verified} requests reused a connection", flush=True)
    check_name = ('s3-' if options.s3 else '') + ('tenants-' if options.tenants else '') + ('runner-' if options.runner else '') + 'checks.json'
    (OUT/check_name).write_text(json.dumps({'binary_sha256':binary_sha256, 'runner_sha256':runner_sha256, 'real_macos_runner':options.runner, 'native_http':True,'domain_isolation':options.tenants,'hard_restart':True,'durable_browser_session':True, 's3_restore':options.s3, 'sqlite_zip64_backup': True, 'archive_upload_and_recovery': True, 'portable_restore': True, 'native_lease': True, 'legacy_direct_restore': True, 'replica_point_restore': options.s3, 'http_during_s3_wait': options.s3, 'corrupt_restore_preserves_live_state': True},indent=2)+'\n')

if __name__ == '__main__':
    main()
