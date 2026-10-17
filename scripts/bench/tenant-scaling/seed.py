#!/usr/bin/env python3
"""Seeds the ts_base template the tenant-scaling runs copy (issues #32/#33).

usage: seed.py <server-binary> <workdir>

Signs up 16 companies on a fresh database (the most any run needs), so
every run starts from the same settled template. Leaves `ts_base` for
scale.py to copy with CREATE DATABASE ... TEMPLATE.
"""
import sys
import time

import os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "deadline-drain"))
from common import Client, Server, createdb, dropdb  # noqa: E402

BIN, WORK = sys.argv[1], sys.argv[2]
DB = "ts_base"
PORT = 18202

# The template is an empty settled history: every run signs up,
# provisions and mirrors its own companies, so any CompanySignedUp rows
# baked in here would only confuse the run's own lifecycle-replicator
# (which can't even see this template's event feed - its tokens are
# minted by the run's server).
dropdb(DB)
createdb(DB)
server = Server(BIN, DB, PORT, f"{WORK}/seed.log")
try:
    time.sleep(10)  # let the server run its loops once against the empty history
finally:
    server.stop()
print("ts_base ready")
