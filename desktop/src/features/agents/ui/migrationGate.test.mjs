import assert from "node:assert/strict";
import test from "node:test";

import { backendUnchanged, migrationGate } from "./migrationGate.ts";

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

// ── backendUnchanged ────────────────────────────────────────────────────────

test("a different destination is always a change", () => {
  assert.equal(backendUnchanged(local, provider), false);
  assert.equal(backendUnchanged(provider, local), false);
  assert.equal(
    backendUnchanged(provider, { type: "provider", id: "blox", config: {} }),
    false,
  );
});

test("staying put with untouched settings is not a change", () => {
  assert.equal(backendUnchanged(local, { type: "local" }), true);
  assert.equal(
    backendUnchanged(
      { type: "provider", id: "sprites", config: { idle_seconds: 600 } },
      { type: "provider", id: "sprites", config: { idle_seconds: 600 } },
    ),
    true,
  );
});

test("editing the current provider's settings is a change", () => {
  // The regression: `set_managed_agent_backend` accepts same-provider with a
  // new config as a real transition (save, then redeploy), and the dialog
  // renders those settings as editable fields. Comparing only the provider id
  // left "Migrate agent" disabled and the supported path unreachable.
  assert.equal(
    backendUnchanged(
      { type: "provider", id: "sprites", config: { idle_seconds: 7200 } },
      { type: "provider", id: "sprites", config: { idle_seconds: 600 } },
    ),
    false,
  );
});

test("adding or dropping a settings key is a change", () => {
  assert.equal(
    backendUnchanged(
      { type: "provider", id: "sprites", config: {} },
      { type: "provider", id: "sprites", config: { idle_seconds: 600 } },
    ),
    false,
  );
  assert.equal(
    backendUnchanged(
      { type: "provider", id: "sprites", config: { idle_seconds: 600 } },
      { type: "provider", id: "sprites", config: {} },
    ),
    false,
  );
});

test("settings are compared by value, not by key order or identity", () => {
  assert.equal(
    backendUnchanged(
      { type: "provider", id: "sprites", config: { a: 1, b: "two" } },
      { type: "provider", id: "sprites", config: { b: "two", a: 1 } },
    ),
    true,
  );
  // Nested values compare structurally rather than by reference.
  assert.equal(
    backendUnchanged(
      { type: "provider", id: "sprites", config: { tags: ["x", "y"] } },
      { type: "provider", id: "sprites", config: { tags: ["x", "y"] } },
    ),
    true,
  );
  assert.equal(
    backendUnchanged(
      { type: "provider", id: "sprites", config: { tags: ["x"] } },
      { type: "provider", id: "sprites", config: { tags: ["y"] } },
    ),
    false,
  );
});

test("a string and the number it coerces to are different settings", () => {
  // `coerceConfigValues` turns the draft's strings back into schema types, so
  // a surviving string here means the schema changed under the record — a
  // real difference, not a formatting one.
  assert.equal(
    backendUnchanged(
      { type: "provider", id: "sprites", config: { idle_seconds: "600" } },
      { type: "provider", id: "sprites", config: { idle_seconds: 600 } },
    ),
    false,
  );
});
