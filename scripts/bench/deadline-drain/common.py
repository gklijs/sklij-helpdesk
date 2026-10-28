"""Shared helpers for the deadline-drain benchmark (issue #28).

See README.md in this directory for how the pieces fit together. Only the
Python standard library is used, so this runs on a bare box.
"""
import http.client
import json
import os
import re
import signal
import subprocess
import time

PGBIN = os.environ.get("PGBIN", os.path.expanduser("~/.theseus/postgresql/18.6.0/bin"))
PGHOST = os.environ.get("PGHOST", "127.0.0.1")
PGPORT = os.environ.get("PGPORT", "5440")
# The master key file the bench servers share (skilj 0.2.0 keeps the key
# in a key provider). Created by the first server; one per bench process.
KEY_FILE = os.environ.get(
    "ENCRYPTION_KEY_FILE", f"/tmp/skilj-helpdesk-bench-{os.getpid()}.keys.json")
PGUSER = os.environ.get("PGUSER", "postgres")
SCHEMA = "bc_helpdesk"


def psql(sql, db, check=True):
    """Runs `sql` through psql, unaligned/tuples-only; returns stdout."""
    out = subprocess.run(
        [f"{PGBIN}/psql", "-h", PGHOST, "-p", PGPORT, "-U", PGUSER, "-d", db,
         "-v", "ON_ERROR_STOP=1", "-AtX", "-c", sql],
        capture_output=True, text=True)
    if check and out.returncode != 0:
        raise RuntimeError(f"psql failed: {out.stderr.strip()}\n{sql}")
    return out.stdout


def createdb(name, template=None):
    args = [f"{PGBIN}/createdb", "-h", PGHOST, "-p", PGPORT, "-U", PGUSER]
    if template:
        args += ["-T", template]
    subprocess.run(args + [name], check=True)


def dropdb(name):
    subprocess.run([f"{PGBIN}/dropdb", "-h", PGHOST, "-p", PGPORT, "-U", PGUSER,
                    "--if-exists", name], check=True)


TOKEN = re.compile(r"^  (\w+): ([0-9a-f]{32}\.[0-9a-f]{64})$")


class Server:
    """A release `server` on its own database, with its printed command tokens."""

    def __init__(self, binary, db, port, log_path, extra_env=None):
        env = dict(os.environ)
        env.update({
            "DATABASE_URL": f"postgres://{PGUSER}@{PGHOST}:{PGPORT}/{db}",
            # One key file per bench run: skilj creates it on the first
            # start, and every server on the run's databases reads it.
            "ENCRYPTION_KEY_FILE": KEY_FILE,
            "PORT": str(port),
            "RUST_LOG": env.get("RUST_LOG", "warn"),
        })
        env.update(extra_env or {})
        self.port = port
        self.log_path = log_path
        self.log = open(log_path, "w")
        self.started = time.time()
        self.proc = subprocess.Popen([binary], env=env, stdout=self.log,
                                     stderr=subprocess.STDOUT)
        self.tokens = self._read_tokens()
        self._wait_listening()

    def _read_tokens(self):
        deadline = time.time() + 180
        while time.time() < deadline:
            tokens, section = {}, None
            with open(self.log_path) as f:
                for line in f:
                    if line.startswith("command tokens"):
                        section = "cmd"
                    elif line.startswith("\x1b") or line[:4].isdigit():
                        continue  # a log line interleaved with the token listing
                    elif line and not line.startswith(" "):
                        section = None
                    m = TOKEN.match(line.rstrip("\n"))
                    if section == "cmd" and m:
                        tokens.setdefault(m.group(1), m.group(2))
            if "ApplyMacro" in tokens:
                return tokens
            if self.proc.poll() is not None:
                raise RuntimeError(f"server exited early, see {self.log_path}")
            time.sleep(0.25)
        self.proc.kill()
        raise RuntimeError("server never printed its tokens")

    def _wait_listening(self):
        while True:
            try:
                c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=5)
                c.request("GET", "/")
                c.getresponse().read()
                c.close()
                return
            except OSError:
                time.sleep(0.2)

    def rss_kb(self):
        try:
            with open(f"/proc/{self.proc.pid}/status") as f:
                for line in f:
                    if line.startswith("VmRSS:"):
                        return int(line.split()[1])
        except OSError:
            pass
        return None

    def hwm_kb(self):
        try:
            with open(f"/proc/{self.proc.pid}/status") as f:
                for line in f:
                    if line.startswith("VmHWM:"):
                        return int(line.split()[1])
        except OSError:
            pass
        return None

    def stop(self):
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGINT)
            try:
                self.proc.wait(timeout=60)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()


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
        for attempt in range(3):
            if self.conn is None:
                self.conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=120)
            t0 = time.perf_counter()
            try:
                self.conn.request("POST", "/v1/commands/trigger", body, headers)
                resp = self.conn.getresponse()
                data = resp.read()
            except (OSError, http.client.HTTPException):
                self.conn.close()
                self.conn = None
                if attempt == 2:
                    raise
                continue
            ms = (time.perf_counter() - t0) * 1000
            accepted = False
            if resp.status == 200:
                try:
                    accepted = bool(json.loads(data).get("accepted", False))
                except ValueError:
                    pass
            return resp.status, accepted, ms


def ticket_payload(ticket_id, company_id, requester="cust-1"):
    return {"ticket_id": ticket_id, "company_id": company_id, "requester_id": requester,
            "logged_by_staff_id": None, "title": "t", "description": "d",
            "priority": "medium", "requester_name": None, "requester_email": None}


def deadline_counts(db):
    """{(schedule_name, status): count} for the helpdesk context."""
    out = psql(f"SELECT schedule_name, status, count(*) FROM {SCHEMA}.deadlines "
               f"GROUP BY 1, 2", db)
    counts = {}
    for line in out.splitlines():
        name, status, n = line.split("|")
        counts[(name, status)] = int(n)
    return counts


def pct(xs, q):
    if not xs:
        return None
    xs = sorted(xs)
    return round(xs[min(len(xs) - 1, int(q * len(xs)))], 2)


def summary(xs):
    return {"n": len(xs), "p50": pct(xs, 0.5), "p95": pct(xs, 0.95),
            "p99": pct(xs, 0.99), "max": round(max(xs), 2) if xs else None}
