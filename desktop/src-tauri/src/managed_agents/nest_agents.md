# Buzz Nest

Your persistent workspace. Created once by the Buzz desktop app. The static content above the managed-section markers is regenerated on upgrades — add custom notes below the markers or in separate files.

## Directory Layout

| Dir | Purpose |
|-----|---------|
| `GUIDES/` | Actionable runbooks synthesized from research |
| `PLANS/` | Planning documents for work in progress |
| `RESEARCH/` | Findings, notes, and reference material |
| `WORK_LOGS/` | Session logs — what was tried, learned, decided |
| `OUTBOX/` | Shareable docs for external readers (no frontmatter) |
| `REPOS/` | Source checkouts. Work in an existing local checkout when one exists; clone here only when none does |
| `.scratch/` | Temporary working files — treat as disposable between sessions |

Filenames: `ALL_CAPS_WITH_UNDERSCORES.md` (e.g., `OAUTH_FLOW_NOTES.md`).

The bundled CLI is your primary tool interface — run its `--help` command for usage. The CLI skill file has the full reference.

## Checking Out a Branch or PR

**Reuse a checkout path; don't mint one per PR.** A fresh clone or `git worktree add` at a *new* path is a cold Rust environment, not just a cold `target/`. Two reasons, and sharing a `target/` only fixes the second:

- Hermit pins `CARGO_HOME` inside whichever directory it is activated in, so a new path re-downloads the whole registry (~800 crates, ~800 MB).
- Cargo keys a local crate's artifacts on its **absolute path**, so a new path recompiles every workspace crate even against a shared `target/`.

Measured on a sprite: ~4 GB and a full cold build per throwaway checkout, thrown away when the PR was done.

So `git fetch` and `git checkout` an existing checkout instead of creating another one. Where `buzz-workspace` is on your PATH (remote agents), let it pick the checkout for you:

```bash
cd "$(buzz-workspace pr/19)"      # a PR, by number
cd "$(buzz-workspace main)"       # a branch, tag, or sha
buzz-workspace list               # which slots hold what
```

It recycles a small pool of fixed-path slots against a shared cargo cache, never touches `target/`, and will not recycle a slot that has uncommitted work. Each hand-out claims its slot for an hour; if you're going to call `buzz-workspace` repeatedly for the same ref, `export BUZZ_WORKSPACE_CLAIM=<anything-unique>` once so your own later calls are recognized as yours instead of refused. `buzz-workspace release <ref>` gives up a claim early.

## Knowledge File Conventions

Files in `GUIDES/`, `PLANS/`, `RESEARCH/`, `WORK_LOGS/` should include YAML frontmatter:

```yaml
---
title: "Always Quoted Title"
tags: [lowercase-hyphenated]
status: active
created: 2026-01-15
---
```

**Status values:** `active` | `superseded` | `stale` | `draft`

> ⚠️ Title **must** be quoted — unquoted colons can break YAML parsing.

## Core Guidelines

- **Local first** — check `RESEARCH/`, `GUIDES/`, `PLANS/` before external searches
- **Write findings down** — if you research something, save it to `RESEARCH/`
- **Cite sources** — no claim without a path, link, or reference
- **Don't overwrite** — append or create new files; don't silently clobber existing work
- **`.scratch/` is disposable** — don't rely on it across sessions
- **Stay on task** — only stage files relevant to your current work

## Git Commit Attribution

Git authorship, co-authorship, DCO sign-off, and cryptographic signing are separate claims. Follow repository-local rules and the authorizing human's explicit directions; do not infer attribution from repository ownership or from who requested, approved, or reviewed the work.

- **Author:** use the person or agent required by the applicable policy. If no policy specifies an author, use the identity that actually authored the change.
- **Co-authors:** add `Co-authored-by` only for other people or agents who materially authored the change. Request, approval, review, or accountability alone is not co-authorship.
- **DCO:** add `Signed-off-by` only when repository policy requires that identity's DCO certification. A sign-off is not an approval marker.
- **Identity:** resolve required identities from trusted local configuration or explicit verified direction; never hard-code or guess them. A managed runtime may make effective `git config user.*` values identify the agent. Stop and ask if a required identity cannot be established.
- **Signing:** use only the signing key configured for the committing identity. Never use another person's signing key.
- **Verify before pushing:** inspect every outgoing commit against the actual upstream or base and confirm its attribution matches the applicable policy.

A repository may require an accountable human as author and the implementing agent as co-author. An agent-owned repository may use the agent as author and require no human trailer. In both cases, repository-local policy controls.

<!-- BEGIN BUZZ MANAGED — regenerated automatically, do not edit below -->
## Active Agents

*(No agents deployed yet. Add agents in the Buzz desktop app.)*

<!-- END BUZZ MANAGED -->
