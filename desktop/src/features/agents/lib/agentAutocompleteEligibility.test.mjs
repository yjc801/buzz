import assert from "node:assert/strict";
import test from "node:test";

import {
  coalesceAgentAutocompleteCandidates,
  filterAdmittedMentionPubkeys,
  filterCachedAgentSuggestions,
  getAdmittedMemberAgentPubkeys,
  getAgentMentionAdmission,
  getMentionableAgentPubkeys,
  getSharedChannelIds,
  isAgentIdentityInAllowedList,
  isAgentMentionChannelType,
  managedAgentBelongsToCommunity,
  relayAgentCanRespondInChannel,
  relayAgentIsSharedWithUser,
  shouldHideAgentFromMentions,
  uniqueAutocompleteLabels,
} from "./agentAutocompleteEligibility.ts";

const CURRENT_PUBKEY = "a".repeat(64);
const OWNER_PUBKEY = "b".repeat(64);
const OTHER_OWNER_PUBKEY = "c".repeat(64);
const PUB_A = "1".repeat(64);
const PUB_B = "2".repeat(64);
const PUB_C = "3".repeat(64);
const PUB_D = "4".repeat(64);

function coalesce(candidates, options = {}) {
  return coalesceAgentAutocompleteCandidates(candidates, {
    currentPubkey: CURRENT_PUBKEY,
    getLabel: (candidate) => candidate.displayName,
    ...options,
  });
}

function makeAgent(overrides = {}) {
  return {
    pubkey: PUB_A,
    displayName: "Pinky",
    isAgent: true,
    isMember: false,
    ...overrides,
  };
}

test("getSharedChannelIds: includes only active joined channels", () => {
  assert.deepEqual(
    getSharedChannelIds([
      { id: "joined", isMember: true, archivedAt: null },
      { id: "not-joined", isMember: false, archivedAt: null },
      { id: "archived", isMember: true, archivedAt: "2026-01-01T00:00:00Z" },
    ]),
    new Set(["joined"]),
  );
});

test("relayAgentIsSharedWithUser: accepts shared anyone agents and rejects unshared ones", () => {
  const sharedChannelIds = new Set(["general"]);

  assert.equal(
    relayAgentIsSharedWithUser(
      { respondTo: "anyone", respondToAllowlist: [], channelIds: ["general"] },
      sharedChannelIds,
    ),
    true,
  );
  assert.equal(
    relayAgentIsSharedWithUser(
      {
        respondTo: "owner-only",
        respondToAllowlist: [],
        channelIds: ["general"],
      },
      sharedChannelIds,
    ),
    false,
  );
  assert.equal(
    relayAgentIsSharedWithUser(
      { respondTo: "anyone", respondToAllowlist: [], channelIds: ["other"] },
      sharedChannelIds,
    ),
    false,
  );
});

test("relayAgentIsSharedWithUser: accepts allowlist agents for the current user", () => {
  const sharedChannelIds = new Set(["general"]);

  assert.equal(
    relayAgentIsSharedWithUser(
      {
        respondTo: "allowlist",
        respondToAllowlist: [OTHER_OWNER_PUBKEY, CURRENT_PUBKEY.toUpperCase()],
        channelIds: ["other"],
      },
      sharedChannelIds,
      CURRENT_PUBKEY,
    ),
    true,
  );
  assert.equal(
    relayAgentIsSharedWithUser(
      {
        respondTo: "allowlist",
        respondToAllowlist: [OTHER_OWNER_PUBKEY],
        channelIds: ["general"],
      },
      sharedChannelIds,
      CURRENT_PUBKEY,
    ),
    false,
  );
});

test("relayAgentCanRespondInChannel: requires exact channel membership and viewer access", () => {
  const agent = {
    respondTo: "allowlist",
    respondToAllowlist: [CURRENT_PUBKEY],
    channelIds: ["general"],
  };

  assert.equal(
    relayAgentCanRespondInChannel(agent, "general", CURRENT_PUBKEY),
    true,
  );
  assert.equal(
    relayAgentCanRespondInChannel(agent, "other", CURRENT_PUBKEY),
    false,
  );
  assert.equal(
    relayAgentCanRespondInChannel(agent, "general", OTHER_OWNER_PUBKEY),
    false,
  );
});

test("managedAgentBelongsToCommunity: unscoped record is never hidden", () => {
  // null/blank communityRelayUrl = shared identity, offered everywhere.
  const directoryAgentPubkeys = new Set();
  for (const communityRelayUrl of [undefined, null, "", "   "]) {
    assert.equal(
      managedAgentBelongsToCommunity({
        agent: { pubkey: PUB_A, communityRelayUrl },
        directoryAgentPubkeys,
        activeCommunityRelayUrl: "wss://one.example",
      }),
      true,
    );
  }
});

test("managedAgentBelongsToCommunity: directory presence outranks a foreign binding", () => {
  // Registered in THIS community's kind:10100 directory => it has actually
  // run here, whatever community it is bound to (#2122 permits this).
  assert.equal(
    managedAgentBelongsToCommunity({
      agent: {
        pubkey: PUB_A,
        communityRelayUrl: "wss://other.communities.example",
      },
      directoryAgentPubkeys: new Set([PUB_A]),
      activeCommunityRelayUrl: "wss://one.example",
    }),
    true,
  );
});

test("managedAgentBelongsToCommunity: bound elsewhere and unregistered here is excluded", () => {
  assert.equal(
    managedAgentBelongsToCommunity({
      agent: {
        pubkey: PUB_A,
        communityRelayUrl: "wss://other.communities.example",
      },
      directoryAgentPubkeys: new Set([PUB_B]),
      activeCommunityRelayUrl: "wss://one.example",
    }),
    false,
  );
});

test("managedAgentBelongsToCommunity: canonical-spelling differences still match", () => {
  // The stored value is canonical, but the active community's relayUrl is
  // user-entered and may be spelled differently.
  assert.equal(
    managedAgentBelongsToCommunity({
      agent: { pubkey: PUB_A, communityRelayUrl: "wss://one.example" },
      directoryAgentPubkeys: new Set(),
      activeCommunityRelayUrl: "WSS://One.Example:443/",
    }),
    true,
  );
});

test("managedAgentBelongsToCommunity: fails open while the community is unresolved", () => {
  assert.equal(
    managedAgentBelongsToCommunity({
      agent: {
        pubkey: PUB_A,
        communityRelayUrl: "wss://other.communities.example",
      },
      directoryAgentPubkeys: new Set(),
      activeCommunityRelayUrl: null,
    }),
    true,
  );
});

test("getMentionableAgentPubkeys: same-named identities bound per community do not leak", () => {
  // The reported bug: three separate identities all named "Bumble", one per
  // community. Only the one bound to the active community is mentionable; the
  // other two would produce a mention no local harness answers.
  const result = getMentionableAgentPubkeys({
    activeCommunityRelayUrl: "wss://devenish.communities.example",
    currentPubkey: CURRENT_PUBKEY,
    eligibilityScope: { type: "managed-only" },
    managedAgents: [
      {
        pubkey: PUB_A,
        communityRelayUrl: "wss://devenish.communities.example",
      },
      { pubkey: PUB_B, communityRelayUrl: "wss://yjc.communities.example" },
      {
        pubkey: PUB_C,
        communityRelayUrl: "wss://openvelvet.communities.example",
      },
    ],
    relayAgents: [],
    sharedChannelIds: new Set(),
  });

  assert.deepEqual(result, new Set([PUB_A]));
});

test("getMentionableAgentPubkeys: managed-only scope still excludes foreign-community records", () => {
  // Regression guard for the original defect: managed agents were seeded into
  // the result set BEFORE the eligibilityScope switch, so even "managed-only"
  // admitted every record in the global store.
  const result = getMentionableAgentPubkeys({
    activeCommunityRelayUrl: "wss://one.example",
    currentPubkey: CURRENT_PUBKEY,
    eligibilityScope: { type: "managed-only" },
    managedAgents: [
      { pubkey: PUB_A },
      { pubkey: PUB_D, communityRelayUrl: "wss://other.communities.example" },
    ],
    relayAgents: [],
    sharedChannelIds: new Set(),
  });

  assert.deepEqual(result, new Set([PUB_A]));
});

test("getMentionableAgentPubkeys: keeps managed agents and shared relay agents", () => {
  const result = getMentionableAgentPubkeys({
    activeCommunityRelayUrl: "wss://one.example",
    eligibilityScope: { type: "community" },
    managedAgents: [{ pubkey: PUB_A }],
    currentPubkey: CURRENT_PUBKEY,
    relayAgents: [
      {
        pubkey: PUB_B,
        respondTo: "anyone",
        respondToAllowlist: [],
        channelIds: ["general"],
      },
      {
        pubkey: PUB_C,
        respondTo: "allowlist",
        respondToAllowlist: [CURRENT_PUBKEY],
        channelIds: ["other"],
      },
      {
        pubkey: PUB_D,
        respondTo: "anyone",
        respondToAllowlist: [],
        channelIds: ["other"],
      },
    ],
    sharedChannelIds: new Set(["general"]),
  });

  assert.deepEqual(result, new Set([PUB_A, PUB_B, PUB_C]));
});

test("getMentionableAgentPubkeys: scopes channel composers and fails closed without context", () => {
  const relayAgents = [
    {
      pubkey: PUB_B,
      respondTo: "allowlist",
      respondToAllowlist: [CURRENT_PUBKEY],
      channelIds: ["general"],
    },
  ];
  const base = {
    activeCommunityRelayUrl: "wss://one.example",
    currentPubkey: CURRENT_PUBKEY,
    managedAgents: [{ pubkey: PUB_A }],
    relayAgents,
    sharedChannelIds: new Set(["general"]),
  };

  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      eligibilityScope: { type: "channel", channelId: "general" },
    }),
    new Set([PUB_A, PUB_B]),
  );
  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      eligibilityScope: { type: "channel", channelId: "other" },
    }),
    new Set([PUB_A]),
  );
  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      eligibilityScope: { type: "managed-only" },
    }),
    new Set([PUB_A]),
  );
});

test("autocomplete helper extraction preserves safe filtering and labels", () => {
  assert.equal(isAgentMentionChannelType("stream"), true);
  assert.equal(isAgentMentionChannelType("forum"), true);
  assert.equal(isAgentMentionChannelType("dm"), false);
  assert.equal(isAgentMentionChannelType(null), false);

  assert.deepEqual(
    uniqueAutocompleteLabels([
      { displayName: " Alice ", personaName: "alice" },
      { displayName: null, secondaryLabel: "Bob" },
      { displayName: "BOB" },
    ]),
    ["Alice", "Bob"],
  );

  const person = { pubkey: PUB_A, isAgent: false };
  const admittedAgent = { pubkey: PUB_B.toUpperCase(), isAgent: true };
  const removedAgent = { pubkey: PUB_C, isAgent: true };
  const persona = { isAgent: true };
  assert.deepEqual(
    filterCachedAgentSuggestions(
      [person, admittedAgent, removedAgent, persona],
      [{ pubkey: PUB_B, isAgent: true }],
    ),
    [person, admittedAgent, persona],
  );
});

test("isAgentIdentityInAllowedList: keeps people and only explicitly allowed agent identities", () => {
  const allowedAgentPubkeys = new Set([PUB_A]);

  assert.equal(
    isAgentIdentityInAllowedList(
      { isAgent: false, pubkey: PUB_B },
      allowedAgentPubkeys,
    ),
    true,
  );
  assert.equal(
    isAgentIdentityInAllowedList(
      { isAgent: true, pubkey: PUB_A.toUpperCase() },
      allowedAgentPubkeys,
    ),
    true,
  );
  assert.equal(
    isAgentIdentityInAllowedList(
      { isAgent: true, pubkey: PUB_B },
      allowedAgentPubkeys,
    ),
    false,
  );
});

test("shouldHideAgentFromMentions: never hides non-agents", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      ownerOnly: false,
      isAgent: false,
      isMember: false,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set([PUB_A]),
    }),
    false,
  );
});

test("shouldHideAgentFromMentions: shows invocable agents even when non-member", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      ownerOnly: false,
      isAgent: true,
      isMember: false,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set([PUB_A]),
      directoryAgentPubkeys: new Set([PUB_A]),
    }),
    false,
  );
});

test("shouldHideAgentFromMentions: hides non-member non-invocable agents", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      ownerOnly: false,
      isAgent: true,
      isMember: false,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set(),
    }),
    true,
  );
});

test("shouldHideAgentFromMentions: hides member agents with an explicit not-invocable directory entry (Fizz)", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      ownerOnly: false,
      isAgent: true,
      isMember: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set([PUB_A]),
    }),
    true,
  );
});

test("shouldHideAgentFromMentions: hides member agents without an affirmative directory grant", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      ownerOnly: false,
      isAgent: true,
      isMember: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set(),
    }),
    true,
  );
});

test("shouldHideAgentFromMentions: hides unknown member agents while directories load", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      ownerOnly: false,
      isAgent: true,
      isMember: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set(),
      directoryReady: false,
    }),
    true,
  );
});

test("shouldHideAgentFromMentions: hides mentionable member agents while directories load", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      ownerOnly: false,
      isAgent: true,
      isMember: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set([PUB_A]),
      directoryAgentPubkeys: new Set(),
      directoryReady: false,
    }),
    true,
  );
});

test("shouldHideAgentFromMentions: shows non-agent members while directories load", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      ownerOnly: false,
      isAgent: false,
      isMember: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set([PUB_A]),
      directoryReady: false,
    }),
    false,
  );
});

test("shouldHideAgentFromMentions: hides unknown member agents after empty directories settle", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      ownerOnly: false,
      isAgent: true,
      isMember: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set(),
      directoryReady: true,
    }),
    true,
  );
});

test("shouldHideAgentFromMentions: hides agents while owner policy loads", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      isAgent: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set([PUB_A]),
      directoryReady: true,
      ownerOnly: undefined,
    }),
    true,
  );
});

test("member agents: shouldHideAgentFromMentions itself has no member bypass", () => {
  // `shouldHideAgentFromMentions` requires membership in
  // `mentionableAgentPubkeys` unconditionally — it takes no `isMember` input
  // at all. The member bypass lives one layer up, in
  // `getAdmittedMemberAgentPubkeys`, which folds live channel membership into
  // the `mentionableAgentPubkeys` set it passes down before calling this
  // function (see that function's doc comment) rather than teaching this
  // lower-level gate about membership. So on a bare call like this one, a
  // channel-member agent absent from `mentionableAgentPubkeys` is hidden,
  // same as `isAgentIdentityInAllowedList` — do not reintroduce
  // `isAgentIdentityInAllowedList` as a second gate in the mention picker
  // (see the comment on its call site in useMentions.ts) on the assumption
  // it is stricter; today it is not.
  const shared = {
    isAgent: true,
    isMember: true,
    pubkey: PUB_A,
    mentionableAgentPubkeys: new Set(),
    directoryAgentPubkeys: new Set(),
    ownerOnly: false,
  };

  assert.equal(
    shouldHideAgentFromMentions(shared),
    true,
    "a channel-member agent absent from mentionableAgentPubkeys is hidden",
  );
  assert.equal(
    isAgentIdentityInAllowedList({ isAgent: true, pubkey: PUB_A }, new Set()),
    false,
    "isAgentIdentityInAllowedList rejects it too — the two now agree",
  );
});

test("getAdmittedMemberAgentPubkeys: admits the member agent the picker shows", () => {
  // The picker's member branch and the send path's agent classification must
  // agree, or an agent the user just picked is treated as an ordinary person
  // once the message sends (no audience promotion, no Huddle enrollment).
  assert.deepEqual(
    [
      ...getAdmittedMemberAgentPubkeys({
        memberAgentPubkeys: [PUB_A],
        isArchived: () => false,
        mentionableAgentPubkeys: new Set([PUB_A]),
        isManagedAgent: () => false,
        getOwnerPubkey: () => null,
        currentPubkey: CURRENT_PUBKEY,
        directoryReady: true,
        ownerOnly: false,
      }),
    ],
    [PUB_A],
  );
});

test("getAdmittedMemberAgentPubkeys: admits a member with no directory/managed record when owner-only is off", () => {
  // A member flagged as an agent but with no kind:10100 entry and no managed
  // record is still mentionable — membership itself proves eligibility.
  // Requiring directory presence here would hide every channel-member agent
  // that hasn't published a full profile.
  assert.deepEqual(
    [
      ...getAdmittedMemberAgentPubkeys({
        memberAgentPubkeys: [PUB_B],
        isArchived: () => false,
        mentionableAgentPubkeys: new Set(),
        isManagedAgent: () => false,
        getOwnerPubkey: () => null,
        currentPubkey: CURRENT_PUBKEY,
        directoryReady: true,
        ownerOnly: false,
      }),
    ],
    [PUB_B],
  );
});

test("getAdmittedMemberAgentPubkeys: drops what the hide rule and archive gate reject", () => {
  assert.deepEqual(
    [
      ...getAdmittedMemberAgentPubkeys({
        // PUB_A: archived.
        // PUB_B: no managed/directory record, owner-only ON with no known
        //   owner — the membership bypass still fails closed on unresolved
        //   owner-only policy, same as any other agent.
        // PUB_C: owner-only and owned by someone else.
        // PUB_D: owner-only, but a managed agent — exempt from owner-only.
        memberAgentPubkeys: [PUB_A, PUB_B, PUB_C, PUB_D],
        isArchived: (pubkey) => pubkey === PUB_A,
        mentionableAgentPubkeys: new Set([PUB_C, PUB_D]),
        isManagedAgent: (pubkey) => pubkey === PUB_D,
        getOwnerPubkey: (pubkey) =>
          pubkey === PUB_C ? OTHER_OWNER_PUBKEY : null,
        currentPubkey: CURRENT_PUBKEY,
        directoryReady: true,
        ownerOnly: true,
      }),
    ],
    [PUB_D],
  );
});

test("getAdmittedMemberAgentPubkeys: normalizes before gating and emitting", () => {
  const mixedCase = "Ab".repeat(32);
  const normalized = mixedCase.toLowerCase();

  assert.deepEqual(
    [
      ...getAdmittedMemberAgentPubkeys({
        memberAgentPubkeys: [mixedCase],
        isArchived: () => false,
        mentionableAgentPubkeys: new Set([normalized]),
        isManagedAgent: () => false,
        getOwnerPubkey: () => null,
        currentPubkey: CURRENT_PUBKEY,
        directoryReady: true,
        ownerOnly: false,
      }),
    ],
    [normalized],
  );
  assert.deepEqual(
    [
      ...getAdmittedMemberAgentPubkeys({
        memberAgentPubkeys: [mixedCase],
        isArchived: () => true,
        mentionableAgentPubkeys: new Set(),
        isManagedAgent: () => false,
        getOwnerPubkey: () => null,
        currentPubkey: CURRENT_PUBKEY,
        directoryReady: true,
        ownerOnly: false,
      }),
    ],
    [],
  );
});

test("shouldHideAgentFromMentions: normalizes the pubkey before lookup", () => {
  const mixedCase = "Ab".repeat(32);
  const normalized = mixedCase.toLowerCase();

  assert.equal(
    shouldHideAgentFromMentions({
      ownerOnly: false,
      isAgent: true,
      pubkey: mixedCase,
      mentionableAgentPubkeys: new Set([normalized]),
    }),
    false,
  );
});

test("getAgentMentionAdmission: owner-only requires current verified ownership", () => {
  const common = {
    isAgent: true,
    isManagedAgent: false,
    pubkey: PUB_A,
    currentPubkey: CURRENT_PUBKEY,
    mentionableAgentPubkeys: new Set([PUB_A]),
    directoryReady: true,
    ownerOnly: true,
  };

  assert.equal(
    getAgentMentionAdmission({ ...common, ownerPubkey: CURRENT_PUBKEY }),
    "allow",
  );
  assert.equal(
    getAgentMentionAdmission({ ...common, ownerPubkey: OTHER_OWNER_PUBKEY }),
    "deny",
  );
  assert.equal(
    getAgentMentionAdmission({ ...common, ownerPubkey: null }),
    "unknown",
  );
});

test("getAgentMentionAdmission: unresolved directory state stays unknown", () => {
  assert.equal(
    getAgentMentionAdmission({
      isAgent: true,
      isManagedAgent: false,
      pubkey: PUB_A,
      currentPubkey: CURRENT_PUBKEY,
      ownerPubkey: CURRENT_PUBKEY,
      mentionableAgentPubkeys: new Set([PUB_A]),
      directoryReady: false,
      ownerOnly: false,
    }),
    "unknown",
  );
});

test("filterAdmittedMentionPubkeys: rechecks agent admission without dropping people", () => {
  assert.deepEqual(
    filterAdmittedMentionPubkeys(
      [PUB_A, PUB_B, PUB_C],
      new Set([PUB_A, PUB_B]),
      new Set([PUB_B]),
    ),
    [PUB_B, PUB_C],
  );
});

test("coalesceAgentAutocompleteCandidates: keeps agents with the same persona id distinct", () => {
  const first = makeAgent({ pubkey: PUB_A, personaId: "pinky" });
  const second = makeAgent({
    pubkey: PUB_B,
    personaId: "pinky",
    isMember: true,
  });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});

test("coalesceAgentAutocompleteCandidates: keeps agents with the same owner and name distinct", () => {
  const first = makeAgent({ pubkey: PUB_A, ownerPubkey: OWNER_PUBKEY });
  const second = makeAgent({
    pubkey: PUB_B,
    ownerPubkey: OWNER_PUBKEY,
    isMember: true,
  });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});

test("coalesceAgentAutocompleteCandidates: keeps same-name agents with different owners distinct", () => {
  const first = makeAgent({ pubkey: PUB_A, ownerPubkey: OWNER_PUBKEY });
  const second = makeAgent({
    pubkey: PUB_B,
    ownerPubkey: OTHER_OWNER_PUBKEY,
  });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});

test("coalesceAgentAutocompleteCandidates: keeps owner-less same-name agents distinct", () => {
  const first = makeAgent({ pubkey: PUB_A });
  const second = makeAgent({ pubkey: PUB_B });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});

test("coalesceAgentAutocompleteCandidates: keeps owner-less managed same-name agents distinct", () => {
  const first = makeAgent({ pubkey: PUB_A, isManagedAgent: true });
  const second = makeAgent({ pubkey: PUB_B, isManagedAgent: true });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});

test("coalesceAgentAutocompleteCandidates: keeps current-owner same-name agents distinct", () => {
  const first = makeAgent({ pubkey: PUB_A, ownerPubkey: CURRENT_PUBKEY });
  const second = makeAgent({
    pubkey: PUB_B,
    ownerPubkey: CURRENT_PUBKEY,
    isManagedAgent: true,
  });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});

test("coalesceAgentAutocompleteCandidates: coalesces repeated source rows for the same pubkey", () => {
  const first = makeAgent({ pubkey: PUB_A });
  const second = makeAgent({
    pubkey: PUB_A.toUpperCase(),
    isMember: true,
  });

  assert.deepEqual(coalesce([first, second]), [second]);
});

test("coalesceAgentAutocompleteCandidates: leaves non-agents alone", () => {
  const first = makeAgent({ pubkey: PUB_A, isAgent: false });
  const second = makeAgent({ pubkey: PUB_B, isAgent: false });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});
