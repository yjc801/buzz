import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { resolveAgentAvailability } from "./useAgentAvailability.ts";
import {
  getManagedAgentPrimaryActionLabel,
  isManagedAgentActive,
} from "./managedAgentControlActions.ts";
import { AgentRuntimeAvatarControl } from "../ui/AgentRuntimeAvatarControl.tsx";

const deployed = {
  status: "deployed",
  backend: { type: "provider", id: "fixture" },
  backendAgentId: "retained-receipt",
};

for (const presence of ["online", "away", "offline", undefined]) {
  test(`retained deployment receipt does not supply availability (${presence})`, () => {
    const availability = resolveAgentAvailability(presence, true, true);
    assert.equal(availability, presence ?? "offline");
    // Controls retain their existing routing. Offline is not permission to
    // spawn a second body, nor proof that a shutdown message succeeded.
    assert.equal(isManagedAgentActive(deployed), true);
    // Fork divergence (see `isManagedAgentLive`): the retained receipt is the
    // control-plane axis and never the live one, so a provider agent's primary
    // control is routed by PRESENCE, not by `status === "deployed"`. Upstream
    // routes it off the receipt and expects "Shutdown" here; this fork ships
    // the presence-routed label because a `deployed` record whose harness is
    // gone would otherwise offer Shutdown forever and never offer Deploy,
    // leaving a dead remote agent unrecoverable. The presence-carrying calls
    // below are the routing the app actually performs.
    assert.equal(
      getManagedAgentPrimaryActionLabel(deployed, availability),
      availability === "online" || availability === "away"
        ? "Shutdown"
        : "Deploy",
    );
    const html = renderToStaticMarkup(
      createElement(AgentRuntimeAvatarControl, {
        activeTestId: "active",
        isActive: true,
        availability,
        isStarting: false,
        label: "Agent",
        startTestId: "start",
        onStart() {},
      }),
    );
    assert.doesNotMatch(html, /is running/);
    assert.match(
      html,
      new RegExp(
        `Agent: ${availability[0].toUpperCase()}${availability.slice(1)}`,
      ),
    );
    assert.equal(html.includes("bg-emerald-500"), availability === "online");
    assert.doesNotMatch(html, /data-testid="start"/);
  });
}

for (const [loaded, connected] of [
  [false, true],
  [true, false],
  [false, false],
]) {
  test(`unavailable presence is unknown, not cached online (${loaded}, ${connected})`, () => {
    const availability = resolveAgentAvailability("online", loaded, connected);
    assert.equal(availability, undefined);
    const html = renderToStaticMarkup(
      createElement(AgentRuntimeAvatarControl, {
        activeTestId: "active",
        isActive: true,
        availability,
        isStarting: false,
        label: "Agent",
        startTestId: "start",
        onStart() {},
      }),
    );
    assert.match(html, /Availability unknown/);
    assert.doesNotMatch(html, /bg-emerald-500|is running/);
  });
}

for (const lifecycle of ["running", "stopped"]) {
  test(`local ${lifecycle} controls remain independent of online presence`, () => {
    const agent = { status: lifecycle, backend: { type: "local" } };
    const isActive = isManagedAgentActive(agent);
    assert.equal(
      getManagedAgentPrimaryActionLabel(agent),
      isActive ? "Stop" : "Start agent",
    );
    const html = renderToStaticMarkup(
      createElement(AgentRuntimeAvatarControl, {
        activeTestId: "active",
        isActive,
        availability: "online",
        isStarting: false,
        label: "Local Agent",
        startTestId: "start",
        onStart() {},
      }),
    );
    assert.equal(html.includes('data-testid="start"'), false);
    assert.equal(html.includes('data-testid="active"'), true);
  });
}

for (const availability of ["online", "away"]) {
  test(`stale restart and runtime error cannot hide stopped ${availability} presence`, () => {
    const html = renderToStaticMarkup(
      createElement(AgentRuntimeAvatarControl, {
        activeTestId: "active",
        startTestId: "start",
        errorTestId: "error",
        isActive: false,
        isStarting: false,
        requiresRestart: true,
        errorLabel: "Previous startup failed",
        availability,
        label: "Agent",
        onStart() {},
      }),
    );
    assert.match(html, /data-testid="active"/);
    assert.doesNotMatch(
      html,
      /data-testid="start"|data-testid="error"|<button/,
    );
  });
}
