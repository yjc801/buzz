import assert from "node:assert/strict";
import { after, afterEach, before, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

const CURRENT = "a".repeat(64);
const AGENT = "b".repeat(64);
const HUMAN = "c".repeat(64);

// The relay's revalidation answer. Empty is what a revoked agent looks like —
// identical to what an agent the directory never listed looks like.
let relayAgentsResponse = [];

// The raw kind:10100 row `revalidate_relay_agents` returns, in the snake_case
// shape tauriRelayAgents maps from. Answering in #general to anyone is what
// makes AGENT mentionable rather than merely present.
const LISTED_AGENT = {
  pubkey: AGENT,
  name: "agent",
  agent_type: "managed",
  channels: ["general"],
  channel_ids: ["general"],
  capabilities: [],
  status: "running",
  respond_to: "anyone",
  respond_to_allowlist: [],
};

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    window: dom.window,
  });
  dom.window.__TAURI_INTERNALS__ = {
    invoke: async (command) =>
      command === "revalidate_relay_agents" ? relayAgentsResponse : [],
  };
});

afterEach(async () => {
  relayAgentsResponse = [];
  const { cleanup } = await import("@testing-library/react");
  cleanup();
});

after(() => dom.window.close());

async function renderRevalidator(initialDirectory) {
  const { renderHook } = await import("@testing-library/react");
  const { useAgentMentionRevalidation } = await import(
    "./agentMentionRevalidation.ts"
  );
  return renderHook(
    ({ directoryAgentPubkeys }) =>
      useAgentMentionRevalidation({
        agentPubkeys: new Set([AGENT]),
        knownDirectoryAgentPubkeys: directoryAgentPubkeys,
        refetchMembers: async () => ({
          data: [{ pubkey: AGENT }],
          error: null,
        }),
        getSelectedAgentPubkeys: () => new Set([AGENT]),
        activeCommunityRelayUrl: null,
        currentPubkey: CURRENT,
        eligibilityScope: { type: "channel", channelId: "general" },
        sharedChannelIds: new Set(["general"]),
        refetchManagedAgents: async () => ({ data: [], error: null }),
      }),
    { initialProps: { directoryAgentPubkeys: initialDirectory } },
  );
}

test("send denies an agent revoked after the directory cache refreshed to empty", async () => {
  // Picker render: the polled relay-agent query still lists AGENT.
  const { rerender, result } = await renderRevalidator(new Set([AGENT]));
  // AGENT's kind:10100 record is revoked and the query successfully refetches
  // before Send, dropping AGENT from the live directory view.
  rerender({ directoryAgentPubkeys: new Set() });

  assert.deepEqual(await result.current([HUMAN, AGENT]), [HUMAN]);
});

test("send still admits a member agent the directory never listed", async () => {
  // Control for the case above: same empty directory view and same empty
  // revalidation, but AGENT was never listed, so nothing was revoked and the
  // lenient member branch must still admit it. This is also what proves the
  // denial above comes from directory provenance rather than from an
  // unreachable relay (which would fail both cases closed).
  const { rerender, result } = await renderRevalidator(new Set());
  rerender({ directoryAgentPubkeys: new Set() });

  assert.deepEqual(await result.current([HUMAN, AGENT]), [HUMAN, AGENT]);
});

test("a targeted revalidation's evidence survives into the next revalidation", async () => {
  // The full polled directory cache stays empty throughout: AGENT's kind:10100
  // record is published too recently for the five-minute poll to have picked it
  // up, so a targeted send-time revalidation is the only view that ever sees
  // it. Both calls below happen on one send — the composer revalidates once
  // before the media upload and again after it.
  const { result } = await renderRevalidator(new Set());

  relayAgentsResponse = [LISTED_AGENT];
  assert.deepEqual(await result.current([HUMAN, AGENT]), [HUMAN, AGENT]);

  // The record is revoked during the upload window, so the second revalidation
  // returns empty. That is a revocation, not a never-listed member, and only
  // the first call's result proves the difference.
  relayAgentsResponse = [];
  assert.deepEqual(await result.current([HUMAN, AGENT]), [HUMAN]);
});
