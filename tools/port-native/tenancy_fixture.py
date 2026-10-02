"""Cross-domain authorization and durable separation on one native listener."""
import json
import time
import urllib.error

SECONDARY = {'Host': 'beta.spin.test', 'Origin': 'http://beta.spin.test'}

def exercise(call, boot, primary_cookie, primary_token, snapshot_digest):
    deadline = time.monotonic() + 90
    while True:
        try:
            status = json.loads(call('/api/auth/status', extra_headers=SECONDARY)[2])
            break
        except urllib.error.HTTPError as error:
            if error.code != 503 or time.monotonic() >= deadline:
                raise
            time.sleep(.1)
    if boot == 0:
        assert not status['configured'], 'primary setup leaked to secondary domain'
        code, headers, _ = call('/api/auth/setup', {'username':'second-domain', 'password':'separate-native-password-42'}, extra_headers=SECONDARY)
        assert code == 201
    else:
        assert status['configured'], 'secondary database did not survive recovery'
        code, headers, _ = call('/api/auth/login', {'username':'second-domain', 'password':'separate-native-password-42'}, extra_headers=SECONDARY)
        assert code == 200
    cookie = headers['Set-Cookie'].split(';')[0]
    state = json.loads(call('/api/state', cookie=cookie, extra_headers=SECONDARY)[2])
    assert state['current_user']['username'] == 'second-domain'
    token = json.loads(call('/api/runners/token', cookie=cookie, extra_headers=SECONDARY)[2])['token']
    assert token != primary_token, 'domains share a generated worker token'
    for path, cookie_header, fields in [
        ('/api/state', primary_cookie, SECONDARY),
        ('/api/state', cookie, {}),
        ('/api/snapshots/'+snapshot_digest+'?offset=0', None, dict(SECONDARY, Origin='', Authorization='Bearer '+primary_token)),
    ]:
        try:
            call(path, cookie=cookie_header, extra_headers=fields)
            raise AssertionError('foreign-domain credentials were accepted')
        except urllib.error.HTTPError as error:
            assert error.code == 401, error.code
    try:
        call('/api/snapshots/'+snapshot_digest+'?offset=0', extra_headers=dict(SECONDARY, Origin='', Authorization='Bearer '+token))
        raise AssertionError('primary snapshot leaked into the secondary database')
    except urllib.error.HTTPError as error:
        assert error.code == 404, error.code
    print(f'PASS native domain isolation: users, browser sessions, worker tokens, blobs; boot={boot}', flush=True)
