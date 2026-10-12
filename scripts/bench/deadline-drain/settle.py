#!/usr/bin/env python3
"""Runs a server idle on <db> until every background loop has caught up.

usage: settle.py <server-binary> <db> <log>

seed.py stops its server as soon as the deadline rows are in place, but a
50k-ticket burst leaves other loops behind - notably the
`HelpdeskResolutionToArticleSuggestion` route into `knowledge_base` and its
`KnowledgeBaseReview` projection. A template left like that makes every run
start with minutes of catch-up that stalls foreground commands (see the
report). This waits until every context's sequence head and projection
cursors have stopped moving for 20s, then stops the server.
"""
import sys
import time

from common import Server, psql

BIN, DB, LOG = sys.argv[1:4]
STATE_SQL = """
SELECT string_agg(x, ' ' ORDER BY x) FROM (
  SELECT 'helpdesk:' || (SELECT next_value FROM bc_helpdesk.sequence) || '/' ||
         (SELECT min(caught_up_to) FROM bc_helpdesk.projections) || '/' ||
         (SELECT coalesce(min(caught_up_to), 0) FROM bc_helpdesk.projection_partition_progress) || '/' ||
         (SELECT coalesce(string_agg(last_dispatched_sequence::text, ','), '') FROM bc_helpdesk.cross_context_route_cursors) AS x
  UNION ALL
  SELECT 'knowledge_base:' || (SELECT next_value FROM bc_knowledge_base.sequence) || '/' ||
         (SELECT min(caught_up_to) FROM bc_knowledge_base.projections)
) s
"""

server = Server(BIN, DB, 18193, LOG)
t0 = time.time()
last, since = None, time.time()
while True:
    state = psql(STATE_SQL, DB).strip()
    if state != last:
        print(f"  +{time.time() - t0:.0f}s {state}", flush=True)
        last, since = state, time.time()
    elif time.time() - since > 20:
        break
    time.sleep(2)
server.stop()
print(f"settled after {time.time() - t0:.0f}s: {last}")
