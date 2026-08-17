import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { JSDOM } from "jsdom";

// ── useChannelLinks regression: mismatching replacement closes synchronously ──
//
// detectPrefixQuery's single-word fast path matches ANY boundary-prefixed
// token, whether or not it matches a known channel name. updateChannelQuery's
// synchronous close guard only fired on a `null` match, so replacing an open
// "#general" query with a non-matching "#zzzz" left the stale "general"
// suggestions open for the full CHANNEL_QUERY_DEBOUNCE_MS window — long enough
// for a fast Enter to be swallowed by handleChannelKeyDown and insert the
// stale suggestion instead of submitting. (PR #73 review, Alex.)

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

before(() => {
  Object.assign(globalThis, {
    document: dom.window.document,
    HTMLElement: dom.window.HTMLElement,
    IS_REACT_ACT_ENVIRONMENT: true,
    window: dom.window,
  });
});

after(() => dom.window.close());

const CHANNEL_QUERY_DEBOUNCE_MS = 120;

function channel(overrides = {}) {
  return {
    id: "ch-general",
    name: "general",
    channelType: "stream",
    visibility: "open",
    description: "",
    topic: null,
    purpose: null,
    memberCount: 0,
    memberPubkeys: [],
    lastMessageAt: null,
    archivedAt: null,
    participants: [],
    participantPubkeys: [],
    isMember: true,
    ttlSeconds: null,
    ttlDeadline: null,
    ...overrides,
  };
}

test("replacing an open #general query with a non-matching #zzzz closes the popup synchronously", async () => {
  const React = await import("react");
  const { act, cleanup, renderHook } = await import("@testing-library/react");
  const { ChannelNavigationProvider } = await import(
    "@/shared/context/ChannelNavigationContext.tsx"
  );
  const { useChannelLinks } = await import("./useChannelLinks.ts");

  const channels = [channel()];
  const wrapper = ({ children }) =>
    React.createElement(ChannelNavigationProvider, { channels }, children);

  try {
    const { result } = renderHook(() => useChannelLinks(), { wrapper });

    act(() => {
      result.current.updateChannelQuery("hi #general", 11);
    });
    await act(async () => {
      await new Promise((resolve) =>
        setTimeout(resolve, CHANNEL_QUERY_DEBOUNCE_MS + 20),
      );
    });
    assert.equal(
      result.current.isChannelOpen,
      true,
      "popup opens for a matching query after the debounce settles",
    );

    // Select-all + retype: the whole "#general" query is replaced with a
    // non-matching "#zzzz" in one keystroke, mirroring a fast edit-and-submit.
    act(() => {
      result.current.updateChannelQuery("hi #zzzz", 8);
    });

    assert.equal(
      result.current.isChannelOpen,
      false,
      "popup must close synchronously — before the debounce fires — when the " +
        "new query cannot match any known channel, or a fast Enter in the " +
        "debounce window gets swallowed and inserts the stale suggestion",
    );
  } finally {
    cleanup();
  }
});
