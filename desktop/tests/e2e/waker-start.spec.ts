/**
 * E2E spec for Start on a Remote-wake agent: the start is handed to the
 * community's buzz-waker (a signed start request) instead of deployed from
 * this machine, and the user is told the agent comes online in a while.
 *
 * Covers:
 *  - Start on a Remote-wake provider agent requests a waker start, deploys
 *    nothing here, and says so in a toast
 *  - Start on a provider agent with Remote wake off still deploys directly
 */
import { expect, test } from "@playwright/test";

import { installMockBridge, TEST_IDENTITIES } from "../helpers/bridge";

const REMOTE_AGENT = TEST_IDENTITIES.charlie;

async function startFromAgentsView(page: import("@playwright/test").Page) {
  await page.goto("/");
  await page.getByTestId("open-agents-view").click();
  // The mock identities start out present; an agent that is online offers
  // no Start. Take this one offline first.
  await page.evaluate(
    (pubkey) =>
      window.__BUZZ_E2E_EMIT_MOCK_PRESENCE__?.({ pubkey, status: "offline" }),
    REMOTE_AGENT.pubkey,
  );
  await page.getByTestId(`agent-runtime-start-${REMOTE_AGENT.pubkey}`).click();
}

test("Start on a Remote-wake agent asks the waker instead of deploying", async ({
  page,
}) => {
  await installMockBridge(page, {
    managedAgents: [
      {
        pubkey: REMOTE_AGENT.pubkey,
        name: "Remote Helper",
        status: "not_deployed",
        backend: { type: "provider", id: "sprites", config: {} },
        wakerEnabled: true,
      },
    ],
  });

  await startFromAgentsView(page);

  await expect(
    page
      .locator("[data-sonner-toast]")
      .filter({ hasText: "Asked your community to start Remote Helper" }),
  ).toBeVisible();
  await expect
    .poll(() =>
      page.evaluate(() => window.__BUZZ_E2E_WAKER_START_REQUESTS__?.()),
    )
    .toEqual([REMOTE_AGENT.pubkey]);
});

test("Start with Remote wake off still deploys from here", async ({ page }) => {
  await installMockBridge(page, {
    managedAgents: [
      {
        pubkey: REMOTE_AGENT.pubkey,
        name: "Remote Helper",
        status: "not_deployed",
        backend: { type: "provider", id: "sprites", config: {} },
        wakerEnabled: false,
      },
    ],
  });

  await startFromAgentsView(page);

  await expect
    .poll(() =>
      page.evaluate(
        () =>
          window.__BUZZ_E2E_COMMAND_LOG__?.filter(
            (entry) => entry.command === "start_managed_agent",
          ).length ?? 0,
      ),
    )
    .toBe(1);
  expect(
    await page.evaluate(() => window.__BUZZ_E2E_WAKER_START_REQUESTS__?.()),
  ).toEqual([]);
  await expect(
    page
      .locator("[data-sonner-toast]")
      .filter({ hasText: "Asked your community to start" }),
  ).toHaveCount(0);
});
