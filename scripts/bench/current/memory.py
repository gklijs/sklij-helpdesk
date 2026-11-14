#!/usr/bin/env python3
"""Sustained traffic and post-load idle RSS, with reproducible source metadata.

Uses a NEW dedicated database; requires a disposable PostgreSQL cluster.
Unlike the historical nominal rate, every HTTP result is counted. A worker
waits for each response, so nominal offered rate is an upper bound.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'deadline-drain'))
from common import Server, Client, createdb, dropdb, psql, summary, ticket_payload

ap = argparse.ArgumentParser(description=__doc__)
ap.add_argument('server')
ap.add_argument('out')
ap.add_argument('--seconds', type=int, default=180)
ap.add_argument('--idle-seconds', type=int, default=60)
ap.add_argument('--workers', type=int, default=80)
ap.add_argument('--interval-ms', type=int, default=50)
ap.add_argument('--port', type=int, default=18401)
ap.add_argument('--jemalloc-profile', help='path to a jemalloc library built with profiling support')
ap.add_argument('--docker-container', default=os.environ.get('BENCH_DATABASE_CONTAINER'), help='optional disposable database container for CPU/I/O samples')
ap.add_argument('--database', default=f'helpdesk_memory_{os.getpid()}')
args = ap.parse_args()
Path(args.out).mkdir(parents=True, exist_ok=True)
# createdb fails if it already exists: never drop a contributor's database.
createdb(args.database)
server = None
stop = threading.Event()
rows = []
lock = threading.Lock()
samples = []
previous_cpu = None


def worker(index):
    client = Client(args.port, server.tokens)
    company = f'memory-{index}'
    n = 0
    while not stop.is_set():
        ticket = f'memory-{index}-{n}'
        n += 1
        for command, payload in [
            ('CreateTicket', ticket_payload(ticket, company, f'customer-{index}')),
            ('AssignTicket', {'ticket_id': ticket, 'staff_id': f'staff-{index}'}),
            ('ResolveTicket', {'ticket_id': ticket}),
            ('CloseTicket', {'ticket_id': ticket, 'requester_id': f'customer-{index}'}),
            ('RateTicket', {'ticket_id': ticket, 'requester_id': f'customer-{index}', 'rating': 5, 'comment': 'Thanks'}),
        ]:
            if stop.is_set():
                return
            started = time.monotonic()
            try:
                status, accepted, ms = client.trigger(command, payload)
            except Exception:
                status, accepted, ms = -1, False, (time.monotonic() - started) * 1000
            with lock:
                rows.append((time.monotonic(), status, accepted, ms))
            stop.wait(max(0, args.interval_ms / 1000 - (time.monotonic() - started)))


def sample(phase, began):
    global previous_cpu
    counters = [int(value) for value in Path('/proc/stat').read_text().splitlines()[0].split()[1:9]]
    cpu = (sum(counters), counters[3] + counters[4])
    busy = None
    if previous_cpu and cpu[0] > previous_cpu[0]:
        busy = round(100 * (1 - (cpu[1] - previous_cpu[1]) / (cpu[0] - previous_cpu[0])), 2)
    previous_cpu = cpu
    state = psql("SELECT state, count(*) FROM pg_stat_activity WHERE datname = current_database() GROUP BY state", args.database)
    projection_lag = psql(
        "SELECT name, GREATEST(COALESCE((SELECT max(sequence) FROM bc_helpdesk.events), 0) - COALESCE(caught_up_to, 0), 0) "
        "FROM bc_helpdesk.projections WHERE NOT sync ORDER BY name", args.database)
    database_stats = psql("SELECT numbackends,xact_commit,xact_rollback,blks_read,blks_hit,temp_files,temp_bytes,deadlocks FROM pg_stat_database WHERE datname=current_database()", args.database).strip()
    docker_stats = None
    if args.docker_container:
        docker_stats = json.loads(subprocess.check_output([
            'docker', 'stats', '--no-stream', '--format', '{{json .}}', args.docker_container
        ], text=True))
    samples.append({'seconds': round(time.monotonic() - began, 2), 'phase': phase,
                    'rss_kib': server.rss_kb(), 'hwm_kib': server.hwm_kb(),
                    'db_activity': state.strip(), 'projection_lag': projection_lag.strip(), 'loadavg': os.getloadavg(),
                    'host_cpu_busy_percent': busy, 'database_counters': database_stats, 'database_container': docker_stats,
                    'mem_available_kib': next(int(line.split()[1]) for line in Path('/proc/meminfo').read_text().splitlines() if line.startswith('MemAvailable:'))})
    with Path(args.out, 'memory-samples.jsonl').open('a') as output:
        output.write(json.dumps(samples[-1]) + '\n')
    if len(samples) % 6 == 1:
        print(json.dumps(samples[-1]), flush=True)


try:
    profile_env = {}
    if args.jemalloc_profile:
        profile_dir = Path(args.out).resolve() / 'profiles'
        profile_dir.mkdir(parents=True, exist_ok=True)
        profile_env = {'LD_PRELOAD': args.jemalloc_profile,
                       'MALLOC_CONF': f'prof:true,prof_final:true,lg_prof_interval:30,prof_prefix:{profile_dir}/heap,stats_print:true'}
    server = Server(args.server, args.database, args.port, f'{args.out}/memory-server.log',
                    {'TICKET_ROUTING': 'shared', 'DATABASE_MAX_CONNECTIONS': '80',
                     'HTTP_MAX_IN_FLIGHT_REQUESTS': '70', **profile_env})
    client = Client(args.port, server.tokens)
    for i in range(args.workers):
        status, accepted, _ = client.trigger('SignUpCompany', {
            'company_id': f'memory-{i}', 'name': f'Memory {i}', 'contact_email': 'bench@example.com'})
        if status != 200 or not accepted:
            raise RuntimeError('signup failed')
    started_utc = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
    began = time.monotonic()
    threads = [threading.Thread(target=worker, args=(i,)) for i in range(args.workers)]
    for thread in threads:
        thread.start()
    while time.monotonic() - began < args.seconds:
        sample('load', began)
        time.sleep(5)
    stop.set()
    for thread in threads:
        thread.join()
    load_ended = time.monotonic()
    while time.monotonic() - load_ended < args.idle_seconds:
        sample('idle', began)
        time.sleep(5)
    sources = {}
    for path in sorted([*Path('src').rglob('*.rs'), Path('Cargo.toml'), Path('Cargo.lock')]):
        sources[str(path)] = hashlib.sha256(path.read_bytes()).hexdigest()
    windows = []
    for offset in range(0, args.seconds, 30):
        selected = [r for r in rows if began + offset <= r[0] < began + min(offset + 30, args.seconds)]
        windows.append({'start_seconds': offset, 'responses': len(selected),
                        'accepted': sum(r[2] for r in selected),
                        'latency_ms': summary([r[3] for r in selected])})
    result = {'started_utc': started_utc, 'args': vars(args), 'commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
              'source_sha256': sources, 'server_sha256': hashlib.sha256(Path(args.server).read_bytes()).hexdigest(), 'rustc': subprocess.check_output(['rustc', '--version'], text=True).strip(),
              'platform': platform.platform(), 'cpu': Path('/proc/cpuinfo').read_text().split('model name')[1].split('\n')[0],
              'logical_cpus': os.cpu_count(), 'meminfo': Path('/proc/meminfo').read_text().splitlines()[:3],
              'postgres_settings': psql("SELECT name, setting FROM pg_settings WHERE name IN ('server_version','max_connections','shared_buffers','work_mem','fsync','synchronous_commit','max_wal_size') ORDER BY name", args.database),
              'server_pid': server.proc.pid, 'load_seconds': round(load_ended - began, 2), 'responses': len(rows),
              'accepted': sum(r[2] for r in rows), 'rejected': sum(r[1] == 200 and not r[2] for r in rows),
              'http_errors': {str(code): sum(r[1] == code for r in rows) for code in sorted(set(r[1] for r in rows if r[1] != 200))},
              'latency_ms': summary([r[3] for r in rows]), 'windows': windows, 'samples': samples}
    Path(f'{args.out}/memory.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in ['accepted', 'rejected', 'http_errors', 'latency_ms']}, indent=2))
finally:
    stop.set()
    if server:
        server.stop()
    dropdb(args.database)
