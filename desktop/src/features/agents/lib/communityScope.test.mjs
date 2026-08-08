import assert from "node:assert/strict";
import test from "node:test";

import {
  communityScopesCollide,
  instanceNameTakenInScope,
  relayUrlsMatch,
} from "./communityScope.ts";

test("relayUrlsMatch: canonical equivalence and trimmed fallback", () => {
  assert.equal(
    relayUrlsMatch("wss://one.example", "WSS://One.Example:443/"),
    true,
  );
  assert.equal(relayUrlsMatch("wss://one.example", "wss://two.example"), false);
  // Neither side canonicalizes (not ws/wss) → trimmed string equality.
  assert.equal(relayUrlsMatch(" not-a-url ", "not-a-url"), true);
  assert.equal(relayUrlsMatch("not-a-url", "other"), false);
});

test("communityScopesCollide: unscoped collides with everything", () => {
  assert.equal(communityScopesCollide(null, null), true);
  assert.equal(communityScopesCollide(null, "wss://one.example"), true);
  assert.equal(communityScopesCollide("wss://one.example", undefined), true);
  assert.equal(communityScopesCollide("  ", "wss://one.example"), true);
});

test("communityScopesCollide: bound scopes collide only when equal", () => {
  assert.equal(
    communityScopesCollide("wss://one.example", "wss://one.example"),
    true,
  );
  assert.equal(
    communityScopesCollide("wss://one.example", "wss://two.example"),
    false,
  );
});

test("instanceNameTakenInScope: case-insensitive within a colliding scope", () => {
  const agents = [
    { name: "Bumble", communityRelayUrl: "wss://one.example" },
    { name: "Alex", communityRelayUrl: null },
  ];
  assert.equal(
    instanceNameTakenInScope(agents, "bumble", "wss://one.example"),
    true,
  );
  assert.equal(
    instanceNameTakenInScope(agents, "Bumble", "wss://two.example"),
    false,
  );
  // Unscoped record collides in every community.
  assert.equal(
    instanceNameTakenInScope(agents, "ALEX", "wss://two.example"),
    true,
  );
  // Blank target never collides.
  assert.equal(
    instanceNameTakenInScope(agents, "   ", "wss://one.example"),
    false,
  );
});
