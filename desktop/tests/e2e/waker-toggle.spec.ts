/**
 * E2E spec for the buzz-waker enrolment toggle in an agent's profile
 * settings menu (`UserProfileAgentSettingsMenu`).
 *
 * Covers:
 *  - the toggle only renders for a provider-backend agent, never for local
 *  - enabling it calls `set_managed_agent_waker_enabled` and flips the switch
 *  - disabling it does the same in reverse
 */
import { expect, test } from "@playwright/test";

import { installMockBridge, TEST_IDENTITIES } from "../helpers/bridge";

const PROVIDER_AGENT = TEST_IDENTITIES.charlie;
const LOCAL_AGENT = TEST_IDENTITIES.bob;

async function openAgentProfileSettingsMenu(
  page: import("@playwright/test").Page,
  agentName: string,
) {
  await page.goto("/");
  await page.getByTestId("open-agents-view").click();
  await page
    .getByRole("button", { name: `${agentName} agent profile` })
    .click();
  await expect(page.getByTestId("user-profile-panel")).toBeVisible();
  await page.getByTestId("user-profile-settings-menu-trigger").click();
}

test("waker toggle only renders for a provider-backend agent", async ({
  page,
}) => {
  await installMockBridge(page, {
    managedAgents: [
      {
        pubkey: PROVIDER_AGENT.pubkey,
        name: "Remote Helper",
        status: "not_deployed",
        backend: { type: "provider", id: "sprites", config: {} },
      },
      {
        pubkey: LOCAL_AGENT.pubkey,
        name: "Local Helper",
        status: "stopped",
        backend: { type: "local" },
      },
    ],
  });

  await openAgentProfileSettingsMenu(page, "Remote Helper");
  await expect(
    page.getByTestId(`user-profile-agent-waker-${PROVIDER_AGENT.pubkey}`),
  ).toBeVisible();
  await page.keyboard.press("Escape");

  await openAgentProfileSettingsMenu(page, "Local Helper");
  await expect(
    page.getByTestId(`user-profile-agent-waker-${LOCAL_AGENT.pubkey}`),
  ).toHaveCount(0);
});

test("enabling and disabling the waker toggle calls the Tauri command", async ({
  page,
}) => {
  await installMockBridge(page, {
    managedAgents: [
      {
        pubkey: PROVIDER_AGENT.pubkey,
        name: "Remote Helper",
        status: "not_deployed",
        backend: { type: "provider", id: "sprites", config: {} },
      },
    ],
  });

  await openAgentProfileSettingsMenu(page, "Remote Helper");
  const toggle = page.getByTestId(
    `user-profile-agent-waker-${PROVIDER_AGENT.pubkey}`,
  );
  await expect(toggle).toHaveAttribute("data-state", "unchecked");

  const commandsBeforeEnable = await page.evaluate(
    () => window.__BUZZ_E2E_COMMAND_LOG__?.length ?? 0,
  );
  await toggle.click();
  await expect(toggle).toHaveAttribute("data-state", "checked");
  await expect(
    page
      .locator("[data-sonner-toast]")
      .filter({ hasText: "can now be woken remotely by buzz-waker" }),
  ).toBeVisible();
  await expect
    .poll(async () =>
      page.evaluate((start) => {
        const commands = window.__BUZZ_E2E_COMMAND_LOG__ ?? [];
        return commands
          .slice(start)
          .some(
            (entry) =>
              entry.command === "set_managed_agent_waker_enabled" &&
              (entry.payload as { wakerEnabled?: boolean })?.wakerEnabled ===
                true,
          );
      }, commandsBeforeEnable),
    )
    .toBe(true);

  const commandsBeforeDisable = await page.evaluate(
    () => window.__BUZZ_E2E_COMMAND_LOG__?.length ?? 0,
  );
  await toggle.click();
  await expect(toggle).toHaveAttribute("data-state", "unchecked");
  await expect(
    page
      .locator("[data-sonner-toast]")
      .filter({ hasText: "will no longer be woken by buzz-waker" }),
  ).toBeVisible();
  await expect
    .poll(async () =>
      page.evaluate((start) => {
        const commands = window.__BUZZ_E2E_COMMAND_LOG__ ?? [];
        return commands
          .slice(start)
          .some(
            (entry) =>
              entry.command === "set_managed_agent_waker_enabled" &&
              (entry.payload as { wakerEnabled?: boolean })?.wakerEnabled ===
                false,
          );
      }, commandsBeforeDisable),
    )
    .toBe(true);
});
