import type * as React from "react";
import type { ChannelType, ManagedAgent } from "@/shared/api/types";
import {
  type ImetaMedia,
  mergeOutgoingTags,
} from "@/features/messages/lib/imetaMediaMarkdown";
import type { QueuedMediaAttachment } from "@/features/messages/lib/backgroundMediaUploadStore";
import type { PreparedBackgroundLinkPreviews } from "@/features/messages/lib/linkPreviewPreparationStore";
import type { UseChannelLinksResult } from "@/features/messages/lib/useChannelLinks";
import type { UseEmojiAutocompleteResult } from "@/features/messages/lib/useEmojiAutocomplete";
import type { UseMentionsResult } from "@/features/messages/lib/useMentions";
import type { UseRichTextEditorResult } from "@/features/messages/lib/useRichTextEditor";
import type {
  DraftMentionRef,
  UseDraftsResult,
} from "@/features/messages/lib/useDrafts";
import { normalizePubkey } from "@/shared/lib/pubkey";
import type { CustomEmoji } from "@/shared/lib/remarkCustomEmoji";
import { MENTION_REFERENCE_TAG } from "@/shared/lib/resolveMentionNames";

export { MENTION_REFERENCE_TAG };

export type UseMentionSendFlowOptions = {
  channelId: string | null;
  channelLinks: Pick<UseChannelLinksResult, "clearChannels">;
  channelType: ChannelType | null;
  contentRef: React.MutableRefObject<string>;
  customEmoji: CustomEmoji[];
  drafts: Pick<UseDraftsResult, "loadDraft" | "markDraftSent" | "persistDraft">;
  emojiAutocomplete: Pick<UseEmojiAutocompleteResult, "clearEmojis">;
  mentions: UseMentionsResult;
  onPrepareSendChannel?: (pubkeys?: string[]) => Promise<string | null>;
  onAddressedAgentsComposerCleared?: (pubkeys: readonly string[]) => string;
  onAddressedAgentsSendFailed?: (pubkeys: readonly string[]) => void;
  onAddressedAgentsSendSucceeded?: (
    pubkeys: readonly string[],
    newlyPinnedPubkeys: readonly string[],
  ) => void;
  onSendRef: React.MutableRefObject<
    (
      content: string,
      mentionPubkeys: string[],
      mediaTags?: string[][],
      channelId?: string | null,
      threadContext?: {
        parentEventId: string | null;
        threadHeadId: string | null;
      } | null,
      forceRest?: boolean,
    ) => Promise<void>
  >;
  richText: Pick<
    UseRichTextEditorResult,
    "clearContent" | "setContent" | "restorePlainTextAndFocusEnd"
  >;
  setContent: (content: string) => void;
  setIsEmojiPickerOpen: React.Dispatch<React.SetStateAction<boolean>>;
  setPendingImeta: (pendingImeta: ImetaMedia[]) => void;
  hasUnsavedMedia: () => boolean;
  clearQueuedAttachments: () => void;
  restoreQueuedAttachments: (attachments: QueuedMediaAttachment[]) => void;
  setSpoileredAttachmentUrls?: React.Dispatch<
    React.SetStateAction<Set<string>>
  >;
};

/**
 * A detached managed-agent wake queued while the send path prepared a
 * message. Queued wakes are flushed fire-and-forget only after the relay
 * accepts the publish: firing earlier lets a fast start failure toast "your
 * message was sent" before the publish outcome is known, and every abort
 * path (cancel, readiness error, prompt dismissal, publish rejection) would
 * strand a wake for a message that never landed.
 */
export type QueuedAgentWake = {
  agent: ManagedAgent;
  /**
   * Unix seconds captured at enqueue time — before the publish — so the
   * floor can never exceed the published message's `created_at`. Stamping at
   * flush time instead could push the spawned harness's startup watermark
   * past the very message the floor exists to cover: a background upload
   * makes the enqueue-to-flush gap arbitrarily long.
   */
  replayFloorUnix: number;
};

/** Queue a wake for `agent`, stamping its replay floor now (enqueue time). */
export function enqueueAgentWake(
  queue: QueuedAgentWake[],
  agent: ManagedAgent,
): void {
  queue.push({ agent, replayFloorUnix: Math.floor(Date.now() / 1000) });
}

/**
 * Collapse queued wakes to one per agent, keeping the first: the earliest
 * enqueue carries the earliest replay floor, and the floor is a lower bound,
 * so the first wake covers every later mention in the same send.
 */
export function dedupeQueuedAgentWakes(
  wakes: readonly QueuedAgentWake[],
): QueuedAgentWake[] {
  const seen = new Set<string>();
  return wakes.filter((wake) => {
    const pubkey = normalizePubkey(wake.agent.pubkey);
    if (seen.has(pubkey)) return false;
    seen.add(pubkey);
    return true;
  });
}

export type PendingNonMemberMentionSend = {
  addressedAgentPubkeys: string[];
  inlineAgentMentionPubkeys: string[];
  capturedChannelId: string | null;
  capturedThreadContext: {
    parentEventId: string | null;
    threadHeadId: string | null;
  } | null;
  trimmed: string;
  mentionPubkeys: string[];
  nonMemberPubkeys: string[];
  outgoingTags?: string[][];
  preparedLinkPreviews?: PreparedBackgroundLinkPreviews | null;
  preparedManagedAgents?: ManagedAgent[];
  /**
   * Wakes queued while creating mentioned persona agents, carried on the
   * draft so they survive the non-member prompt and flush with the readiness
   * pass's queue after the publish succeeds — a dismissed prompt drops them.
   */
  queuedAgentWakes?: QueuedAgentWake[];
  readyAgentPubkeys?: string[];
  savedContent: string;
  savedImeta: ImetaMedia[];
  queuedAttachments: QueuedMediaAttachment[];
  savedSpoileredAttachmentUrls: Set<string>;
  sentDraftKey: string | null | undefined;
  recoveryDraftKey: string | null | undefined;
  savedMentionRefs: DraftMentionRef[];
};

export type SendMessageWithMentionFlowInput = {
  addressedAgentPubkeys?: readonly string[];
  capturedChannelId: string | null;
  capturedThreadContext?: PendingNonMemberMentionSend["capturedThreadContext"];
  pendingImeta: ImetaMedia[];
  queuedAttachments?: QueuedMediaAttachment[];
  linkPreviewTags?: string[][];
  preparedLinkPreviews?: PreparedBackgroundLinkPreviews | null;
  sentDraftKey: string | null | undefined;
  recoveryDraftKey: string | null | undefined;
  spoileredAttachmentUrls?: ReadonlySet<string>;
  trimmed: string;
};

export async function resolvePreviewTags(
  draft: Pick<PendingNonMemberMentionSend, "preparedLinkPreviews">,
  mediaTags: string[][] | undefined,
  outgoingTags: string[][] | undefined,
): Promise<string[][] | null> {
  const result = await draft.preparedLinkPreviews?.promise;
  if (result?.status === "cancelled") return null;
  return (
    mergeOutgoingTags(mediaTags, [
      ...(outgoingTags ?? []),
      ...(result?.tags ?? []),
    ]) ?? []
  );
}

export function mergeOutgoingTagsWithReferenceMentions(
  outgoingTags: string[][] | undefined,
  pubkeys: Iterable<string>,
) {
  const normalizedPubkeys = uniqueNormalizedPubkeys(pubkeys);
  if (normalizedPubkeys.length === 0) {
    return outgoingTags;
  }

  return [
    ...(outgoingTags ?? []),
    ...normalizedPubkeys.map((pubkey) => [MENTION_REFERENCE_TAG, pubkey]),
  ];
}

export function getErrorMessage(error: unknown, fallback: string) {
  if (error instanceof Error && error.message) return error.message;
  if (typeof error === "string" && error.trim()) return error;
  if (
    typeof error === "object" &&
    error !== null &&
    "message" in error &&
    typeof error.message === "string" &&
    error.message.trim()
  ) {
    return error.message;
  }
  return fallback;
}

export function formatMessageSendError(error: unknown) {
  return `Message failed to send: ${getErrorMessage(error, "Unknown error")}`;
}

export function uniqueNormalizedPubkeys(pubkeys: Iterable<string>) {
  return [...new Set([...pubkeys].map(normalizePubkey))].filter(Boolean);
}

export function mergeMentionRecipients(
  explicitMentionPubkeys: Iterable<string>,
  addressedAgentPubkeys: Iterable<string>,
) {
  return uniqueNormalizedPubkeys([
    ...explicitMentionPubkeys,
    ...addressedAgentPubkeys,
  ]);
}

export function isManagedAgentRunning(agent: ManagedAgent) {
  return agent.status === "running" || agent.status === "deployed";
}

export function isProviderBackedAgent(agent: ManagedAgent) {
  return agent.backend.type === "provider";
}

export function getNonMemberMentionPubkeys({
  pubkeys,
  channelType,
  hasResolvedMembers,
  memberPubkeys,
}: {
  pubkeys: string[];
  channelType: ChannelType | null;
  hasResolvedMembers: boolean;
  memberPubkeys: ReadonlySet<string>;
}) {
  if (channelType === null || channelType === "dm" || !hasResolvedMembers) {
    return [];
  }

  return uniqueNormalizedPubkeys(pubkeys).filter(
    (pubkey) => !memberPubkeys.has(pubkey),
  );
}
