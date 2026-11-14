#!/usr/bin/env python3
"""Rehearse the 0.3 -> 0.4 upgrade and backup rollback on disposable PostgreSQL.

Build both server binaries first; see docs/upgrade-0.4.md. Evidence includes
private backup, key and log files; publish only the sanitized result.json.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import threading
import time
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--old-server', type=Path, required=True)
    parser.add_argument('--new-server', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--post-migration-tests', action='store_true',
                        help='run integration suites on the restored, migrated database')
    args = parser.parse_args()
    old, new = args.old_server.resolve(), args.new_server.resolve()
    for binary in (old, new):
        if not binary.is_file():
            parser.error(f'build the server binary first: {binary}')
    out = args.output.resolve()
    if out.exists() and any(out.iterdir()):
        parser.error('choose an empty output directory; existing evidence is preserved')
    out.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(out, 0o700)
    container = f'helpdesk-upgrade-{os.getpid()}'
    process = None
    received = {}
    checks = []
    root = Path(__file__).resolve().parents[2]
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        http_port = listener.getsockname()[1]
    base = f'http://127.0.0.1:{http_port}'

    def docker(*command, **kwargs):
        return subprocess.check_output(['docker', *command], **kwargs)

    def sql(db, statement, quiet=False):
        return docker('exec', container, 'psql', '-U', 'postgres', '-d', db,
                      '-v', 'ON_ERROR_STOP=1', '-tAc', statement,
                      stderr=subprocess.DEVNULL if quiet else None).decode().strip()

    def url(db):
        return f'postgres://postgres:postgres@127.0.0.1:{pg_port}/{db}'

    def env(db, **extra):
        result = dict(os.environ)
        for name in ('OIDC_ISSUER_URL', 'OTEL_EXPORTER_OTLP_ENDPOINT', 'ENCRYPTION_MASTER_KEY'):
            result.pop(name, None)
        result.update(DATABASE_URL=url(db), ENCRYPTION_KEY_FILE=str(out/'keys.json'),
                      PORT=str(http_port), BIND_ADDR='127.0.0.1',
                      TICKET_ROUTING='shared', SEED_DEMO_TRAFFIC='0', RUST_LOG='warn')
        result.update(extra)
        return result

    def stop():
        nonlocal process
        if process is not None and process.poll() is None:
            process.send_signal(signal.SIGTERM)
            try:
                process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        process = None

    def start(binary, db, label, **extra):
        nonlocal process
        stop()
        path = out/f'{label}.log'
        with path.open('w') as log:
            process = subprocess.Popen([str(binary)], env=env(db, **extra),
                                       stdout=log, stderr=log)
        deadline = time.monotonic()+120
        while time.monotonic() < deadline:
            text = path.read_text()
            if 'listening on' in text:
                return dict(re.findall(r'^  (\w+): ([a-f0-9]{32}\.[a-f0-9]{64})$', text, re.M))
            if process.poll() is not None:
                raise RuntimeError(f'{label} exited; inspect private log {path}')
            time.sleep(.25)
        raise TimeoutError(f'{label} did not start; inspect {path}')

    def post(path, credential, body):
        request = urllib.request.Request(base+path, json.dumps(body).encode(), headers={
            'authorization': f'Bearer {credential}', 'content-type': 'application/json'})
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)

    def trigger(tokens, command, payload):
        response = post('/v1/commands/trigger', tokens[command], {'payload': payload})
        assert response.get('accepted'), f'{command} rejected: {response}'
        return response

    def administrative(db, mode, label):
        result = subprocess.run([str(new), mode], env=env(
            db, PREFLIGHT_RECONCILIATION_SUBJECT=subject), capture_output=True, text=True,
            timeout=120)
        (out/f'{label}.log').write_text(result.stderr)
        assert result.returncode == 0, f'{mode} failed; inspect {label}.log'
        # Console telemetry can precede the JSON report.
        report = json.loads(result.stdout[result.stdout.index('{'):])
        (out/f'{label}.json').write_text(json.dumps(report, indent=2)+'\n')
        return report

    class Collector(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
            received.setdefault(self.path, bytearray()).extend(body)
            self.send_response(200)
            self.end_headers()

        def log_message(self, *_args):
            pass

    collector = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Collector)
    threading.Thread(target=collector.serve_forever, daemon=True).start()
    try:
        docker('run', '-d', '--name', container, '-e', 'POSTGRES_PASSWORD=postgres',
               '-p', '127.0.0.1::5432', 'postgres:16-alpine')
        pg_port = docker('port', container, '5432/tcp').decode().strip().rsplit(':', 1)[1]
        deadline = time.monotonic()+60
        while True:
            try:
                if sql('postgres', 'SELECT 1', quiet=True) == '1':
                    break
            except subprocess.CalledProcessError:
                if time.monotonic() >= deadline:
                    raise
            time.sleep(.25)
        for db in ('old', 'restored', 'rollback', 'fresh'):
            sql('postgres', f'CREATE DATABASE {db}')
        tokens = start(old, 'old', 'old')
        trigger(tokens, 'SignUpCompany', {'company_id': 'upgrade99', 'name': 'Upgrade rehearsal',
                                        'contact_email': 'upgrade@example.test'})
        trigger(tokens, 'CreateTicket', {'ticket_id': 'upgrade99', 'company_id': 'upgrade99',
                'requester_id': 'upgrade99', 'logged_by_staff_id': None,
                'title': 'Migration private title', 'description': 'Migration private description',
                'priority': 'low'})
        subject = sql('old', "SELECT external_subject FROM roles WHERE external_subject LIKE 'skilj-helpdesk-admin-%' LIMIT 1")
        assert subject, 'old server did not bootstrap the expected reconciliation role'
        # Model a historical protected event whose source text was retained by 0.3.
        sql('old', "UPDATE bc_helpdesk.events SET origin_kind='external_triggered', origin_source_content='historical private source', origin_source_context='email' WHERE event_type_name='TicketCreated'")
        before = sql('old', 'SELECT count(*) FROM bc_helpdesk.events')
        stop()
        backup = docker('exec', container, 'pg_dump', '-U', 'postgres', '-Fc', 'old')
        (out/'before.dump').write_bytes(backup)
        for db in ('restored', 'rollback'):
            subprocess.run(['docker', 'exec', '-i', container, 'pg_restore', '-U', 'postgres',
                            '-d', db, '--exit-on-error'], input=backup, check=True)
        checks.append('backup/restore')
        assert administrative('restored', '--preflight', 'preflight-before')['verdict'] == 'upgrade_needed'
        roles = sql('restored', 'SELECT count(*) FROM roles')
        refused = subprocess.run([str(new)], env=env('restored', SCHEMA_CHANGES='refuse'),
                                 capture_output=True, text=True, timeout=30)
        (out/'refused-before.log').write_text(refused.stdout+refused.stderr)
        assert refused.returncode != 0 and 'schema_upgrade_required' in refused.stderr
        assert sql('restored', 'SELECT count(*) FROM roles') == roles, 'refusal mutated bootstrap roles'
        checks.extend(['preflight', 'refuse before bootstrap'])
        administrative('restored', '--migrate', 'migration')
        assert administrative('restored', '--preflight', 'preflight-after')['verdict'] == 'up_to_date'
        assert sql('restored', 'SELECT count(*) FROM bc_helpdesk.events') == before
        assert sql('restored', "SELECT origin_source_content FROM bc_helpdesk.events WHERE event_type_name='TicketCreated'") == '[redacted]'
        checks.append('protected source rewrite')
        tokens = start(new, 'restored', 'new', SCHEMA_CHANGES='refuse',
            OTEL_EXPORTER_OTLP_ENDPOINT=f'http://127.0.0.1:{collector.server_port}',
            OTEL_METRIC_EXPORT_INTERVAL='500', RUST_LOG='info')
        sql('restored', "UPDATE role_access_mappings SET can_read_sensitive=true WHERE bounded_context='helpdesk'")
        jwt = re.search(r'^  (ey[^\s]+)$', (out/'new.log').read_text(), re.M)[1]

        def graphql(query):
            result = post('/graphql', jwt, {'query': query})
            assert not result.get('errors'), result
            return result['data']

        query = 'query { projection(boundedContext:"helpdesk", name:"CustomerTickets", key:"upgrade99") { ... on helpdesk_CustomerTickets { tickets } } }'
        assert 'Migration private title' in json.dumps(graphql(query))
        trigger(tokens, 'AssignTicket', {'ticket_id': 'upgrade99', 'staff_id': 'staff99'})
        trigger(tokens, 'ResolveTicket', {'ticket_id': 'upgrade99'})
        erased = graphql('mutation { forgetSubject(boundedContext:"helpdesk", subjectKey:"customer", subjectValue:"upgrade99") { status } }')
        assert erased['forgetSubject']['status'] == 'DESTROYED'
        assert 'Migration private title' not in json.dumps(graphql(query))
        checks.extend(['old encrypted read/erasure', 'old ticket assign/resolve'])
        # Await actual batch export instead of assuming a particular SDK
        # scheduling delay or relying on SIGTERM to flush Rust destructors.
        telemetry_paths = ('/v1/metrics', '/v1/traces', '/v1/logs')
        deadline = time.monotonic()+15
        while not all(received.get(path) for path in telemetry_paths) and time.monotonic() < deadline:
            time.sleep(.1)
        stop()
        for path in telemetry_paths:
            assert received.get(path), f'no telemetry received at {path}'
            (out/(path.rsplit('/', 1)[-1]+'.protobuf')).write_bytes(received[path])
        checks.append('OTLP metrics/traces/logs')
        if args.post_migration_tests:
            suites = ('multi_tenant_provisioning', 'tenant_lifecycle_mirroring', 'customer_erasure',
                      'native_deadlines', 'ticket_auto_close_cancel', 'projection_rebuild',
                      'email_channel', 'tenant_channels')
            command = ['cargo', 'test', '--locked']
            for suite in suites:
                command.extend(['--test', suite])
            command.extend(['--', '--nocapture'])
            path = out/'post-migration-tests.log'
            with path.open('w') as log:
                result = subprocess.run(command, cwd=root, env=env('restored',
                    TEST_ENCRYPTION_KEY_FILE=str(out/'keys.json')), stdout=log, stderr=log)
            assert result.returncode == 0, f'post-migration suites failed; inspect {path}'
            assert 'skipping:' not in path.read_text(), 'a post-migration test silently skipped'
            checks.append('post-migration integration suites')
        failed = subprocess.run([str(old)], env=env('restored'), capture_output=True,
                                text=True, timeout=30)
        (out/'old-refused.log').write_text(failed.stdout+failed.stderr)
        assert failed.returncode != 0 and '10' in failed.stderr, '0.3 must refuse migration 0010'
        checks.append('0.3 refused')
        tokens = start(old, 'rollback', 'rollback')
        trigger(tokens, 'AssignTicket', {'ticket_id': 'upgrade99', 'staff_id': 'rollback99'})
        stop()
        checks.append('backup rollback to 0.3')
        administrative('fresh', '--migrate', 'fresh-migration')
        start(new, 'fresh', 'fresh', SCHEMA_CHANGES='refuse')
        stop()
        checks.append('fresh migration/startup')
        result = {'result': 'passed', 'old_events': int(before), 'checks': checks,
                  'binary_sha256': {label: hashlib.sha256(binary.read_bytes()).hexdigest()
                                    for label, binary in [('old', old), ('new', new)]}}
        (out/'result.json').write_text(json.dumps(result, indent=2)+'\n')
        print(f'Upgrade rehearsal passed; private evidence: {out}')
    finally:
        stop()
        collector.shutdown()
        collector.server_close()
        subprocess.run(['docker', 'rm', '-f', container], check=False, stdout=subprocess.DEVNULL)


if __name__ == '__main__':
    main()
