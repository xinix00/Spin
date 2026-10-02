"""A real macOS runner and Docker containers, isolated by a per-test Docker label."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import time
import uuid


def exercise(root, temporary, url, cookie, csrf, token, call):
    binary = root / 'target/release/spin-client'
    if not binary.is_file():
        raise RuntimeError('build the macOS spin-client release before --runner')
    docker = shutil.which('docker')
    if not docker:
        raise RuntimeError('Docker CLI not found')
    version = subprocess.run([docker, 'version', '--format', '{{json .Server}}'], check=True,
                             capture_output=True, text=True, timeout=10)
    if not json.loads(version.stdout):
        raise RuntimeError('Docker daemon is not ready')
    label = 'spin.port_fixture=' + uuid.uuid4().hex
    directory = temporary / 'runner'
    directory.mkdir()
    wrapper = directory / 'docker'
    # Real Docker executes every command. The extra label prevents the temporary
    # server's orphan reconciliation from observing anyone else's containers.
    wrapper.write_text('#!/usr/bin/env python3\nimport os,sys\n'
                       f'docker={docker!r}\nlabel={label!r}\n'
                       'args=sys.argv[1:]\n'
                       'if args and args[0]=="ps": args[1:1]=["--filter","label="+label]\n'
                       'if args and args[0]=="run": args[1:1]=["--label",label]\n'
                       'os.execv(docker,[docker]+args)\n')
    wrapper.chmod(0o700)
    environment = dict(os.environ, SPIN_SERVER=url, SPIN_WORKER_TOKEN=token,
                       SPIN_CLIENT_ID_FILE=str(directory/'identity'),
                       SPIN_WORKER_TOKEN_FILE=str(directory/'token'),
                       SPIN_ENV_DIR=str(directory/'env'), SPIN_DOCKER=str(wrapper),
                       SPIN_CLIENT_NAME='native-port-fixture')
    headers = {'X-Spin-CSRF':csrf}

    def request(path, body=None):
        code, _, payload = call(path, body, cookie, headers)
        return code, json.loads(payload)

    def recording_result(code, result, stage):
        deadline = time.monotonic() + 240
        while code == 202 or result.get('status') == 'running':
            if time.monotonic() > deadline:
                raise AssertionError(f'recording {stage} did not finish')
            time.sleep(.2)
            code, result = request('/api/recordings/'+result['recording_id']+'/'+stage)
        if result.get('status') == 'error':
            raise AssertionError(result)
        return result.get('recording' if stage == 'start' else 'artifact', result)

    with (root/'target/port-native/macos-runner.log').open('w') as log:
        process = subprocess.Popen([str(binary)], env=environment, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                assert process.poll() is None, 'runner exited; inspect macos-runner.log'
                _, state = request('/api/state')
                if state.get('engine', {}).get('available'):
                    break
                time.sleep(.2)
            else:
                raise AssertionError('macOS runner did not register')
            recording = recording_result(*request('/api/recordings', {'kind':'tool', 'name':'native-probe'}), 'start')
            record_id = recording['id']
            code, recorded = request('/api/recordings/'+record_id+'/commands', {'input':"printf 'native-runner-proof' > /opt/spin-proof"})
            assert code == 200 and recorded['commands'][-1]['exit_code'] == 0, recorded
            artifact = recording_result(*request('/api/recordings/'+record_id+'/end', {}), 'seal')
            snapshot = artifact['snapshot']
            assert snapshot['restorable'] and snapshot['digest']
            # Delete only this fixture's sealed image: materialization must pull
            # the archived snapshot back from the native server.
            inspected = subprocess.run([docker,'image','inspect',snapshot['ref']],check=True,capture_output=True,text=True,timeout=20)
            metadata = json.loads(inspected.stdout)[0]
            assert metadata['Config']['Labels'].get('spin.port_fixture') == label.split('=',1)[1]
            subprocess.run([docker,'image','rm',snapshot['ref']],check=True,capture_output=True,timeout=20)
            code, composition = request('/api/use', {'selector':'tool:native-probe'})
            assert code == 201, composition
            container = composition['runtime']['container_id']
            proof = subprocess.run([docker,'exec',container,'cat','/opt/spin-proof'],check=True,capture_output=True,timeout=20)
            assert proof.stdout == b'native-runner-proof'
            code, stopped = request('/api/compositions/'+composition['id']+'/stop', {})
            assert code == 200 and stopped['runtime']['status'] == 'stopped', stopped
            print('PASS real macOS runner: record, execute, seal, archive upload, image deletion, snapshot pull, materialize and stop', flush=True)
        finally:
            process.terminate()
            try:
                process.wait(timeout=12)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
            for args, remove in ((['ps','-aq'],['rm','-f']), (['image','ls','-q'],['image','rm','-f'])):
                listed = subprocess.run([docker,*args,'--filter','label='+label],capture_output=True,text=True,timeout=20,check=True)
                ids = sorted(set(listed.stdout.split()))
                if ids:
                    subprocess.run([docker,*remove,*ids],capture_output=True,timeout=30,check=True)
