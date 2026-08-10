import assert from "node:assert/strict";
import test from "node:test";

import { migrationGate } from "./migrationGate.ts";

const local = { type: "local" };
const provider = { type: "provider", id: "sprites", config: {} };

/** Presence loaded and reporting offline — the shape of a permitted move. */
const quiet = { presenceStatus: "offline", presenceLoaded: true };

test("a stopped local agent may move, and asserts nothing about a remote", () => {
  assert.deepEqual(
    migrationGate({ agent: { backend: local, status: "stopped" }, ...quiet }),
    { allowed: true, remoteConfirmedStopped: false },
  );
});

test("a running local agent is blocked regardless of presence", () => {
  // A local agent can read offline by presence while its process is alive —
  // presence is a relay signal, not a process signal.
  const gate = migrationGate({
    agent: { backend: local, status: "running" },
    ...quiet,
  });
  assert.equal(gate.allowed, false);
  assert.match(gate.reason, /Stop the agent/);
});

test("an offline provider agent may move and asserts stoppedness", () => {
  assert.deepEqual(
    migrationGate({
      agent: { backend: provider, status: "deployed" },
      ...quiet,
    }),
    { allowed: true, remoteConfirmedStopped: true },
  );
});

test("an online provider agent is blocked with the !shutdown instruction", () => {
  const gate = migrationGate({
    agent: { backend: provider, status: "deployed" },
    presenceStatus: "online",
    presenceLoaded: true,
  });
  assert.equal(gate.allowed, false);
  assert.match(gate.reason, /!shutdown/);
});

test("away counts as running — only offline releases the gate", () => {
  const gate = migrationGate({
    agent: { backend: provider, status: "deployed" },
    presenceStatus: "away",
    presenceLoaded: true,
  });
  assert.equal(gate.allowed, false);
});

test("presence not yet loaded fails closed", () => {
  const gate = migrationGate({
    agent: { backend: provider, status: "deployed" },
    presenceStatus: undefined,
    presenceLoaded: false,
  });
  assert.equal(gate.allowed, false);
});

test("presence loaded but absent for this agent fails closed", () => {
  // The riskiest case: the lookup resolved and simply has no entry. Absence is
  // not evidence of offline — an agent whose heartbeat never arrived looks
  // identical to one that never existed.
  const gate = migrationGate({
    agent: { backend: provider, status: "deployed" },
    presenceStatus: undefined,
    presenceLoaded: true,
  });
  assert.equal(gate.allowed, false);
  assert.match(gate.reason, /Can't tell/);
});

test("provider status never substitutes for presence", () => {
  // `not_deployed` is tempting to read as "definitely not running", but it only
  // means no infrastructure is recorded. If presence says online, believe
  // presence.
  const gate = migrationGate({
    agent: { backend: provider, status: "not_deployed" },
    presenceStatus: "online",
    presenceLoaded: true,
  });
  assert.equal(gate.allowed, false);
});
