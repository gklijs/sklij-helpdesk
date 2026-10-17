#!/usr/bin/env node
// Fan-out helper for scripts/bench/read-side/read_side.py (issue #31).
//
// Opens N concurrent graphql-transport-ws connections, subscribes each to
// `allEvents` on the shared helpdesk context, then submits a small series
// of real CreateTicket commands over REST. Reports, per event, how long it
// took each subscriber to receive it and whether every subscriber did.
//
// `ws` is already installed on this box (the same package wscat uses).
"use strict";

const http = require("http");
const WebSocket = require("ws");

function arg(name, fallback) {
  const i = process.argv.indexOf("--" + name);
  return i === -1 ? fallback : process.argv[i + 1];
}

const port = Number(arg("port", 18301));
const jwt = arg("jwt");
const token = arg("token");
const company = arg("company", "rs-small");
const clientCount = Number(arg("clients", 1));
const eventCount = Number(arg("events", 3));
const fromSequence = Number(arg("from-sequence", 0));
if (!jwt || !token) {
  console.error("usage: fanout.js --port P --jwt JWT --token COMMAND_TOKEN ...");
  process.exit(2);
}

const subscription =
  'subscription { allEvents(boundedContext: "helpdesk", eventTypes: ["TicketCreated"], ' +
  `fromSequence: ${fromSequence}) { sequence payload } }`;

const markers = [];
const triggerStarted = [];
const triggerMs = [];
const deliveries = [];
const clients = [];
const errors = [];
const closed = [];
let acked = 0;
let subscribed = 0;
let ready = 0;
let started = false;
const clientReady = new Array(clientCount).fill(false);

function trigger(i) {
  const ticketId = `rs-fanout-${Date.now()}-${i}`;
  markers[i] = ticketId;
  const body = JSON.stringify({
    payload: {
      ticket_id: ticketId,
      company_id: company,
      requester_id: "bench-customer",
      logged_by_staff_id: null,
      title: `fanout ${i}`,
      description: "fan-out",
      priority: "low",
      requester_name: null,
      requester_email: null,
    },
  });
  triggerStarted[i] = Date.now();
  const req = http.request(
    {
      host: "127.0.0.1",
      port,
      path: "/v1/commands/trigger",
      method: "POST",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": "application/json",
        "content-length": Buffer.byteLength(body),
      },
    },
    (res) => {
      let data = "";
      res.on("data", (chunk) => (data += chunk));
      res.on("end", () => {
        triggerMs[i] = Date.now() - triggerStarted[i];
        let accepted = false;
        try {
          accepted = Boolean(JSON.parse(data).accepted);
        } catch (_) {
          // left false; reported below
        }
        if (!accepted) {
          errors.push(`event ${i}: trigger rejected: ${res.statusCode} ${data}`);
        }
        if (i + 1 < eventCount) {
          setTimeout(() => trigger(i + 1), 250);
        } else {
          setTimeout(finish, 3000);
        }
      });
    },
  );
  req.on("error", (e) => errors.push(`event ${i}: ${e}`));
  req.write(body);
  req.end();
}

function finish() {
  const perEvent = [];
  for (let i = 0; i < eventCount; i++) {
    const times = (deliveries[i] || []).filter((x) => typeof x === "number");
    const sorted = [...times].sort((a, b) => a - b);
    const pct = (q) => (sorted.length
      ? sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))]
      : null);
    perEvent.push({
      event: i,
      received: times.length,
      missing: clientCount - times.length,
      trigger_ms: triggerMs[i] ?? null,
      min_ms: sorted.length ? sorted[0] : null,
      p50_ms: pct(0.5),
      p95_ms: pct(0.95),
      max_ms: sorted.length ? sorted[sorted.length - 1] : null,
    });
  }
  for (const ws of clients) {
    try {
      ws.close();
    } catch (_) {
      // already closed
    }
  }
  console.log("FANOUT " + JSON.stringify({
    clients: clientCount,
    events: eventCount,
    from_sequence: fromSequence,
    per_event: perEvent,
    errors,
    closed,
    acked,
    subscribed,
    ready,
  }));
  process.exit(0);
}

for (let i = 0; i < clientCount; i++) {
  const ws = new WebSocket(`ws://127.0.0.1:${port}/graphql`, "graphql-transport-ws");
  clients.push(ws);
  ws.on("open", () => {
    ws.send(JSON.stringify({
      type: "connection_init",
      payload: { Authorization: `Bearer ${jwt}` },
    }));
  });
  ws.on("message", (raw) => {
    let msg;
    try {
      msg = JSON.parse(raw.toString());
    } catch (_) {
      return;
    }
    if (msg.type === "connection_ack") {
      acked++;
      ws.send(JSON.stringify({
        id: "sub",
        type: "subscribe",
        payload: { query: subscription },
      }));
      subscribed++;
      if (subscribed === clientCount) {
        // Do not trigger yet: `fromSequence` replays exactly one existing
        // matching event, and the first `next` on every connection is the
        // proof that its subscription is registered server-side.
      }
    } else if (msg.type === "next") {
      if (msg.payload && msg.payload.errors) {
        errors.push(`subscription errors: ${JSON.stringify(msg.payload.errors)}`);
        return;
      }
      const event = msg.payload && msg.payload.data && msg.payload.data.allEvents;
      if (!event) return;
      if (markers.length === 0) {
        // The readiness replay, not a benchmark event. Count each
        // connection once: a duplicate/multiple replay must not let the
        // barrier release before every subscriber is actually registered.
        if (!clientReady[i]) {
          clientReady[i] = true;
          ready++;
        }
        if (ready === clientCount && !started) {
          started = true;
          setTimeout(() => trigger(0), 250);
        }
        return;
      }
      let payload = {};
      try {
        payload = JSON.parse(event.payload);
      } catch (_) {
        return;
      }
      const idx = markers.indexOf(payload.ticket_id);
      if (idx === -1) return;
      deliveries[idx] = deliveries[idx] || [];
      deliveries[idx].push(Date.now() - triggerStarted[idx]);
    } else if (msg.type === "ping") {
      ws.send(JSON.stringify({ type: "pong" }));
    } else if (msg.type === "error" || msg.type === "connection_error") {
      errors.push(JSON.stringify(msg));
    }
  });
  ws.on("error", (e) => errors.push(String(e)));
  ws.on("close", (code, reason) => {
    if (closed.length < 10) closed.push({ client: i, code, reason: String(reason) });
  });
}

setTimeout(() => {
  errors.push(`timeout: ${acked}/${clientCount} connected, ${subscribed} subscribed`);
  finish();
}, 30000);
