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
//
// A follow-up review round found the first fix incomplete: validating the new
// query against the global channel list proves it matches SOME channel, but
// not that it matches the suggestions currently rendered from the old,
// still-stale `channelQuery` state. Replacing "#general" with another valid
// channel query ("#random") hit the same swallowed-Enter race, just with both
// ends of the replacement being individually valid. (PR #73 review round 2,
// Alex.)
//
// A third round found the "mutual-prefix continuation" fix (query ancestry)
// still wrong: channel matching is substring-based (`includes`), so an
// extension of the open query is not guaranteed to still match. With only
// "general" known, "#gen" -> "#genz" is a valid extension but matches no
// channel. With "agenda" and "general" both known, "#gen" -> "#gene" is a
// valid extension too, but drops "agenda" from the match set. Both reproduce
// the same swallowed-Enter race. (PR #73 review round 3, Alex.)

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

test("replacing an open #general query with a different valid #random query closes the popup synchronously", async () => {
  const React = await import("react");
  const { act, cleanup, renderHook } = await import("@testing-library/react");
  const { ChannelNavigationProvider } = await import(
    "@/shared/context/ChannelNavigationContext.tsx"
  );
  const { useChannelLinks } = await import("./useChannelLinks.ts");

  const channels = [channel(), channel({ id: "ch-random", name: "random" })];
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
    assert.equal(result.current.isChannelOpen, true);
    assert.equal(result.current.channelQuery, "general");

    // Select-all + retype: "#general" is replaced with "#random" in one
    // keystroke. Both are individually valid channel queries, so a guard that
    // only checks "does the new query match some known channel" stays open —
    // but the rendered suggestion is still "general" until the debounce runs.
    act(() => {
      result.current.updateChannelQuery("hi #random", 10);
    });

    assert.equal(
      result.current.isChannelOpen,
      false,
      "popup must close synchronously when switching from one valid channel " +
        "query to a different one — leaving the stale suggestion open lets a " +
        "fast Enter insert the wrong channel",
    );

    await act(async () => {
      await new Promise((resolve) =>
        setTimeout(resolve, CHANNEL_QUERY_DEBOUNCE_MS + 20),
      );
    });
    assert.equal(
      result.current.isChannelOpen,
      true,
      "the debounce still reopens the popup for the new, correct query",
    );
    assert.equal(result.current.channelQuery, "random");
  } finally {
    cleanup();
  }
});

test("extending an open #gen query to a non-matching #genz closes the popup synchronously", async () => {
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
      result.current.updateChannelQuery("hi #gen", 7);
    });
    await act(async () => {
      await new Promise((resolve) =>
        setTimeout(resolve, CHANNEL_QUERY_DEBOUNCE_MS + 20),
      );
    });
    assert.equal(result.current.isChannelOpen, true);
    assert.equal(result.current.channelQuery, "gen");

    // "#genz" is a textual extension of "#gen" (still starts with it), but no
    // known channel matches it — query ancestry alone is not sufficient.
    act(() => {
      result.current.updateChannelQuery("hi #genz", 8);
    });

    assert.equal(
      result.current.isChannelOpen,
      false,
      "an extension of the open query must still close synchronously when " +
        "the extended text matches no known channel",
    );
  } finally {
    cleanup();
  }
});

test("extending an open #gen query to #gene closes the popup when it drops a currently-matching channel", async () => {
  const React = await import("react");
  const { act, cleanup, renderHook } = await import("@testing-library/react");
  const { ChannelNavigationProvider } = await import(
    "@/shared/context/ChannelNavigationContext.tsx"
  );
  const { useChannelLinks } = await import("./useChannelLinks.ts");

  const channels = [
    channel({ id: "ch-agenda", name: "agenda" }),
    channel({ id: "ch-general", name: "general" }),
  ];
  const wrapper = ({ children }) =>
    React.createElement(ChannelNavigationProvider, { channels }, children);

  try {
    const { result } = renderHook(() => useChannelLinks(), { wrapper });

    act(() => {
      result.current.updateChannelQuery("hi #gen", 7);
    });
    await act(async () => {
      await new Promise((resolve) =>
        setTimeout(resolve, CHANNEL_QUERY_DEBOUNCE_MS + 20),
      );
    });
    assert.equal(result.current.isChannelOpen, true);
    assert.deepEqual(
      result.current.channelSuggestions.map((s) => s.name).sort(),
      ["agenda", "general"],
      "both agenda and general substring-match the open #gen query",
    );

    // "#gene" is a textual extension of "#gen" and still matches "general",
    // but "agenda" — one of the currently rendered suggestions — no longer
    // matches. The stale list (and whatever it shows at the selected index)
    // can no longer be trusted.
    act(() => {
      result.current.updateChannelQuery("hi #gene", 8);
    });

    assert.equal(
      result.current.isChannelOpen,
      false,
      "the popup must close synchronously when extending the query drops any " +
        "channel that was part of the currently rendered suggestion set",
    );
  } finally {
    cleanup();
  }
});
