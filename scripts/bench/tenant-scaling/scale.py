#!/usr/bin/env python3
"""Tenant-scaling throughput (issue #32) with a pgbench co-resident (issue #33).

    python3 scale.py <server-bin> <provisioner-bin> <replicator-bin> <label> <out-dir> [options]

Measures aggregate command throughput against 1, 4 and 16 per-company tenant
bounded contexts plus a shared-context baseline, all with a pgbench TPC-B
workload running against a separate database on the same cluster.

Each run: fresh database from the settled template, start server with
TICKET_ROUTING=tenant (or TICKET_ROUTING=shared for the shared baseline), provisioner +
lifecycle-replicator alongside, wait for every tenant to exist and mirror
its lifecycle, init pgbench on a sibling database, then drive foreground
ticket traffic while pgbench runs. Writes <label>.json, <label>-samples.csv
and <label>-latency.csv into <out-dir>.

Reuses ../deadline-drain/common.py (Server, Client, psql), so the same
PGHOST/PGPORT/PGUSER/PGBIN apply (default 127.0.0.1:5440).
"""
import argparse
import csv
import http.client
import json
import os
import re
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "deadline-drain"))
from common import (PGBIN, PGHOST, PGPORT, PGUSER, Server,  # noqa: E402
                    createdb, dropdb, psql, summary)

ap = argparse.ArgumentParser()
ap.add_argument("server")
ap.add_argument("provisioner")
ap.add_argument("replicator")
ap.add_argument("template")
ap.add_argument("label")
ap.add_argument("out")
ap.add_argument("--tenants", type=int, default=1,
                help="companies to provision tenants for (0 = shared baseline)")
ap.add_argument("--fg-workers", type=int, default=16)
ap.add_argument("--fg-interval-ms", type=int, default=50)
ap.add_argument("--seconds", type=float, default=240)
ap.add_argument("--pgbench-clients", type=int, default=4)
ap.add_argument("--pgbench-threads", type=int, default=2)
ap.add_argument("--pgbench-scale", type=int, default=10)
ap.add_argument("--port", type=int, default=18201)
args = ap.parse_args()

os.makedirs(args.out, exist_ok=True)
DB = f"ts_{re.sub(r'[^a-z0-9]', '_', args.label.lower())}"
PGDB = f"{DB}_pgbench"
COMPANIES = [f"ts-{args.label}-{i}" for i in range(args.tenants or 1)]

stop_fg = threading.Event()
stop_sampler = threading.Event()
latencies = []  # (wall_time, command, status, accepted, ms, tenant)
lat_lock = threading.Lock()
samples = []


def graphql(base_url, jwt, query, variables):
    body = json.dumps({"query": query, "variables": variables})
    c = http.client.HTTPConnection("127.0.0.1", args.port, timeout=120)
    c.request("POST", "/graphql", body,
              {"authorization": f"Bearer {jwt}", "content-type": "application/json"})
    resp = c.getresponse()
    data = json.loads(resp.read())
    c.close()
    if resp.status != 200:
        raise RuntimeError(f"graphql http {resp.status}: {data}")
    if data.get("errors"):
        raise RuntimeError(f"graphql errors: {data['errors']}")
    return data["data"]


MINT_CMD = ("mutation Mint($bc: String!, $commandType: String!) { "
            "createCommandToken(boundedContext: $bc, commandTypeName: $commandType) "
            "{ id secret } }")


def mint_token(jwt, tenant, command_type):
    data = graphql(f"http://127.0.0.1:{args.port}", jwt, MINT_CMD,
                   {"bc": tenant, "commandType": command_type})
    tok = data["createCommandToken"]
    return f"{tok['id']}.{tok['secret']}"


COMMANDS = ["CreateTicket", "AssignTicket", "AddInternalNote", "ResolveTicket"]


def provision_and_mirror(server):
    """Sign up every company, provision its tenant via GraphQL, record it,
    and mirror its lifecycle - the synchronous equivalent of what the
    provisioner + lifecycle-replicator binaries do over their poll loops
    (they still run alongside; this just doesn't wait on their 5s ticks).
    Returns {company_id: (tenant_name, {command: token})}."""
    jwt = None
    role = None
    for line in open(server.log_path):
        m = re.match(r"^  export PROVISIONER_TENANT_ADMIN_ROLE_ID=(\S+)$",
                     line.rstrip("\n"))
        if m:
            role = m.group(1)
        m = re.match(r"^  (\S+)$", line.rstrip("\n"))
        if m and len(m.group(1)) > 100 and jwt is None:
            jwt = m.group(1)
    if jwt is None:
        raise RuntimeError("no GraphQL JWT in server log")
    if role is None:
        raise RuntimeError("no PROVISIONER_TENANT_ADMIN_ROLE_ID in server log")
    client = Client(server.port, server.tokens)
    tenants = {}
    for company in COMPANIES:
        status, accepted, _ = client.trigger(
            "SignUpCompany",
            {"company_id": company, "name": company,
             "contact_email": f"ops@{company}.example"})
        if not (status == 200 and accepted):
            raise RuntimeError(f"SignUpCompany {company}: http {status} accepted={accepted}")
        # Tenant names must be legal bounded-context names; the ts-* company
        # ids only use [a-z0-9-], so dashes -> underscores keeps them
        # readable. (Production uses routing::tenant_name_for's digest
        # scheme; these bench companies never collide after normalising.)
        tenant = re.sub(r"[^a-z0-9_]", "_", company.lower().replace("-", "_"))
        data = graphql(f"http://127.0.0.1:{args.port}", jwt,
                       ("mutation Prov($template: String!, $name: String!, $roleId: ID!) { "
                        "createBoundedContextFromTemplate(template: $template, name: $name, "
                        "roleId: $roleId, level: ADMIN, canReadSensitive: false) "
                        "{ name status accessMappings { role { id } } } }"),
                       {"template": "helpdesk", "name": tenant,
                        "roleId": role})
        tenant = data["createBoundedContextFromTemplate"]["name"]
        status, accepted, _ = client.trigger(
            "RecordCompanyTenant", {"company_id": company, "tenant_name": tenant})
        if not (status == 200 and accepted):
            raise RuntimeError(f"RecordCompanyTenant {company}: http {status} accepted={accepted}")
        tokens = {cmd: mint_token(jwt, tenant, cmd) for cmd in COMMANDS}
        tokens["RecordTenantLifecycle"] = mint_token(
            jwt, tenant, "RecordTenantLifecycle")
        status, accepted, _ = Client(server.port, tokens).trigger(
            "RecordTenantLifecycle",
            {"company_id": company, "status": "trialing",
             "source_event_type": "CompanySignedUp"})
        if not (status == 200 and accepted):
            raise RuntimeError(f"RecordTenantLifecycle {company}: http {status} accepted={accepted}")
        tenants[company] = (tenant, tokens)
        print(f"  provisioned {company} -> {tenant}", flush=True)
    return tenants


class Client:
    """One keep-alive REST connection to `/v1/commands/trigger`."""

    def __init__(self, port, tokens):
        self.port, self.tokens = port, tokens
        self.conn = None

    def trigger(self, command, payload):
        """Returns (http_status, accepted, latency_ms)."""
        body = json.dumps({"payload": payload})
        headers = {"authorization": f"Bearer {self.tokens[command]}",
                   "content-type": "application/json"}
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=120)
        t0 = time.perf_counter()
        try:
            c.request("POST", "/v1/commands/trigger", body, headers)
            resp = c.getresponse()
            data = resp.read()
        finally:
            c.close()
        ms = (time.perf_counter() - t0) * 1000
        accepted = False
        if resp.status == 200:
            try:
                accepted = bool(json.loads(data).get("accepted", False))
            except ValueError:
                pass
        return resp.status, accepted, ms


def fg_worker(w, tenants, shared_client):
    if args.tenants:
        company = COMPANIES[w % len(COMPANIES)]
        tenant, tokens = tenants[company]
        client = Client(args.port, tokens)
    else:
        # Shared baseline: one company per worker (signed up in main),
        # each worker sticking to its own so tickets don't collide.
        company = f"ts-{args.label}-co-{w}"
        tenant = None
        client = shared_client
    interval = args.fg_interval_ms / 1000
    n = 0
    nxt = time.time() + interval * w / args.fg_workers
    while not stop_fg.is_set():
        tid = f"ts-{args.label}-{w}-{n}"
        n += 1
        steps = [
            ("CreateTicket", {"ticket_id": tid, "company_id": company,
                              "requester_id": f"{company}-cust-{w % 25}",
                              "logged_by_staff_id": None, "title": "t",
                              "description": "d", "priority": "medium"}),
            ("AssignTicket", {"ticket_id": tid, "staff_id": f"ts-staff-{w % 5}"}),
            ("AddInternalNote", {"ticket_id": tid, "staff_id": f"ts-staff-{w % 5}",
                                 "note": "n"}),
            ("ResolveTicket", {"ticket_id": tid}),
        ]
        for command, payload in steps:
            if stop_fg.is_set():
                return
            delay = nxt - time.time()
            if delay > 0:
                time.sleep(delay)
            nxt += interval
            t = time.time()
            try:
                status, accepted, ms = client.trigger(command, payload)
            except Exception:  # noqa: BLE001 - counted as a transport failure
                status, accepted, ms = -1, False, (time.time() - t) * 1000
            with lat_lock:
                latencies.append((t, command, status, accepted, ms, tenant or "helpdesk"))


def sampler(db_contexts):
    while not stop_sampler.is_set():
        t = time.time()
        try:
            heads = {}
            for ctx in db_contexts:
                heads[ctx] = int(psql(f"SELECT next_value FROM bc_{ctx}.sequence", DB))
            active = int(psql(
                f"SELECT count(*) FROM pg_stat_activity WHERE datname = '{DB}' "
                "AND state = 'active'", "postgres"))
            samples.append({"t": round(t, 3), "heads": heads, "active": active,
                            "load": open("/proc/loadavg").read().split()[:3]})
        except Exception as e:  # noqa: BLE001
            print("sample failed:", e, flush=True)
        time.sleep(max(0, 1.0 - (time.time() - t)))


class Sidecar:
    """A provisioner/lifecycle-replicator child, env-wired from the server log."""

    def __init__(self, binary, log_path, extra_env):
        env = dict(os.environ)
        env.update({"SKILJ_BASE_URL": f"http://127.0.0.1:{args.port}"})
        env.update(extra_env)
        self.log = open(log_path, "w")
        self.proc = subprocess.Popen([binary], env=env, stdout=self.log,
                                     stderr=subprocess.STDOUT)

    def stop(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


def server_exports(log_path, keys=None):
    exports = {}
    for line in open(log_path):
        m = re.match(r"^  export (\w+)=(\S+)$", line.rstrip("\n"))
        if m and (keys is None or m.group(1) in keys):
            exports[m.group(1)] = m.group(2)
    return exports


def run_pgbench():
    dropdb(PGDB)
    createdb(PGDB)
    subprocess.run([f"{PGBIN}/pgbench", "-h", PGHOST, "-p", PGPORT, "-U", PGUSER,
                    "-i", "-s", str(args.pgbench_scale), PGDB], check=True,
                   capture_output=True, text=True)
    log = open(f"{args.out}/{args.label}-pgbench.log", "w")
    proc = subprocess.Popen(
        [f"{PGBIN}/pgbench", "-h", PGHOST, "-p", PGPORT, "-U", PGUSER,
         "-c", str(args.pgbench_clients), "-j", str(args.pgbench_threads),
         "-T", str(int(args.seconds)), "-P", "10", PGDB],
        stdout=log, stderr=subprocess.STDOUT, text=True)
    return proc, log


def parse_pgbench(out):
    m = re.search(r"tps = ([\d.]+)", out)
    tps = float(m.group(1)) if m else None
    m = re.search(r"latency average = ([\d.]+) ms", out)
    lat = float(m.group(1)) if m else None
    return tps, lat


def window(t0, t1):
    with lat_lock:
        rows = [r for r in latencies if t0 <= r[0] < t1]
    ok = [r[4] for r in rows if r[2] == 200 and r[3]]
    res = summary(ok)
    res["seconds"] = round(t1 - t0, 1)
    res["accepted"] = len(ok)
    res["accepted_per_s"] = round(len(ok) / (t1 - t0), 2) if t1 > t0 else None
    res["fired"] = len(rows)
    res["failures"] = sum(1 for r in rows if r[2] != 200)
    res["rejected"] = sum(1 for r in rows if r[2] == 200 and not r[3])
    return res


def main():
    for db in (DB, PGDB):
        dropdb(db)
    createdb(DB, args.template)
    psql("VACUUM ANALYZE", DB)
    print(f"[{args.label}] loadavg before: {open('/proc/loadavg').read().split()[:3]}",
          flush=True)

    extra = {"TICKET_ROUTING": "tenant" if args.tenants else "shared"}
    server = Server(args.server, DB, args.port, f"{args.out}/{args.label}-server.log",
                    {**extra, "DATABASE_MAX_CONNECTIONS": "120",
                     "HTTP_MAX_IN_FLIGHT_REQUESTS": "200"})
    sidecars = []
    tenants = {}
    db_contexts = ["helpdesk"]
    if args.tenants:
        # The server mints fresh event/command tokens at every startup, so
        # the sidecars must start AFTER the run's own server (and against
        # its log) - tokens from any earlier server (e.g. the template
        # seed's) answer 403 against this database.
        exports = server_exports(server.log_path, None)
        sidecars.append(Sidecar(
            args.provisioner, f"{args.out}/{args.label}-provisioner.log",
            {"COMPANY_SIGNED_UP_TOKEN": exports["COMPANY_SIGNED_UP_TOKEN"],
             "RECORD_COMPANY_TENANT_TOKEN": exports["RECORD_COMPANY_TENANT_TOKEN"],
             "PROVISIONER_SUPERADMIN_SUBJECT": exports["PROVISIONER_SUPERADMIN_SUBJECT"],
             "PROVISIONER_TENANT_ADMIN_ROLE_ID":
                 exports["PROVISIONER_TENANT_ADMIN_ROLE_ID"]}))
        sidecars.append(Sidecar(
            args.replicator, f"{args.out}/{args.label}-replicator.log",
            {"COMPANY_SIGNED_UP_TOKEN": exports["COMPANY_SIGNED_UP_TOKEN"],
             "COMPANY_ACTIVATED_TOKEN": exports["COMPANY_ACTIVATED_TOKEN"],
             "COMPANY_EXPIRED_TOKEN": exports["COMPANY_EXPIRED_TOKEN"],
             "REPLICATOR_SUPERADMIN_SUBJECT": exports["REPLICATOR_SUPERADMIN_SUBJECT"]}))
        tenants = provision_and_mirror(server)
        db_contexts += [t for t, _ in tenants.values()]

    pg_proc, pg_log = run_pgbench()
    smp = threading.Thread(target=sampler, args=(db_contexts,))
    smp.start()
    if args.tenants:
        workers = [threading.Thread(target=fg_worker, args=(w, tenants, None))
                   for w in range(args.fg_workers)]
    else:
        # Shared baseline: one company per worker, each worker sticking to
        # its own so tickets don't collide across workers.
        shared_client = Client(server.port, server.tokens)
        for i in range(args.fg_workers):
            company = f"ts-{args.label}-co-{i}"
            status, accepted, _ = shared_client.trigger(
                "SignUpCompany",
                {"company_id": company, "name": company,
                 "contact_email": f"ops@{company}.example"})
            if not (status == 200 and accepted):
                raise RuntimeError(f"SignUpCompany {company}: http {status} accepted={accepted}")
        workers = [threading.Thread(target=fg_worker, args=(w, {}, shared_client))
                   for w in range(args.fg_workers)]
    for w in workers:
        w.start()
    t_start = time.time()
    pg_proc.wait()
    stop_fg.set()
    for w in workers:
        w.join()
    t_end = time.time()
    stop_sampler.set()
    smp.join()
    pg_log.close()

    pg_out = open(f"{args.out}/{args.label}-pgbench.log").read()
    pg_tps, pg_lat = parse_pgbench(pg_out)
    res = {"label": args.label, "args": vars(args), "tenants": list(tenants),
           "window": window(t_start, t_end),
           "pgbench_tps": pg_tps, "pgbench_lat_ms": pg_lat,
           "pgbench_raw": pg_out[-2000:]}
    json.dump(res, open(f"{args.out}/{args.label}.json", "w"), indent=1)
    with open(f"{args.out}/{args.label}-samples.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["t"] + [f"head_{c}" for c in db_contexts] + ["active", "load1"])
        for s in samples:
            w.writerow([s["t"]] + [s["heads"].get(c) for c in db_contexts]
                       + [s["active"], s["load"][0]])
    with open(f"{args.out}/{args.label}-latency.csv", "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["t", "command", "status", "accepted", "ms", "tenant"])
        w.writerows(latencies)
    print(json.dumps({"label": args.label, "accepted_per_s": res["window"]["accepted_per_s"],
                      "pgbench_tps": pg_tps, "pgbench_lat_ms": pg_lat,
                      "failures": res["window"]["failures"],
                      "rejected": res["window"]["rejected"]}, indent=1), flush=True)

    for s in sidecars:
        s.stop()
    server.stop()
    dropdb(DB)
    dropdb(PGDB)


if __name__ == "__main__":
    main()
