# PR size

Every pull request gets a size label from the `PR size` check
(`.github/workflows/pr-size.yml`). The label measures the lines a reviewer has
to read, not the raw diff.

| Label | Counted lines | What happens |
|---|---|---|
| `size/S` | ≤ 200 | Nothing. This is the target. |
| `size/M` | 201–400 | Nothing. |
| `size/L` | 401–800 | A comment asks you to split, or to explain in the description why it stays whole. |
| `size/XL` | > 800 | The check **fails** until you split it or add a `## Why not split` section to the description. |

## Why these numbers

Across 328 reviewed merges in Velvet and this fork (Aug–Sep 2026):

| Lines changed | Mean review rounds | Approved in round one | Median open → merge |
|---|---|---|---|
| 0–49 | 1.3 | 80% | 0.3 h |
| 100–199 | 1.8 | 59% | 0.9 h |
| 200–399 | 1.9 | 40% | 0.9 h |
| 400–799 | 2.3 | 43% | 1.7 h |
| 800–1599 | 2.9 | 26% | 3.1 h |

Pull requests with three or more review rounds were 29% of merges and 57% of
all open-to-merge hours. Round-one approval falls off past about 200 lines,
and rounds climb steeply past 800.

## What counts

Counted: every changed line in a file a reviewer has to read.

Not counted (reported in the comment and the job summary):

- **Tests** — the globs under `tests` in `.github/pr-size.json`. The budget
  must never argue against writing a test.
- **Generated files** — anything the repository's `.gitattributes` marks
  `linguist-generated`, plus the `generated` globs in the config.
- **Lockfiles** — `Cargo.lock`, `pnpm-lock.yaml`, `pubspec.lock` and the like.
- **Binary files and pure renames.**
- **Whole-file deletions** — removing a file is read at a glance.

To exclude a new generated path, mark it `linguist-generated` in
`.gitattributes` (GitHub also collapses it in the diff view) or add a glob to
the config.

## Small is not the goal; coherent is

A pull request is coherent when all of these hold:

1. **One intent.** The title is one sentence with no "and".
2. **Main stays green and shippable** after it merges on its own.
3. **Revertable alone** without breaking a later pull request.
4. **Nothing orphaned.** Every new function or type has a caller or a test in
   the same pull request.
5. **The reviewer can state what would break** if it were wrong.

Small but incoherent — half a feature that does not build alone — is worse
than large and coherent. The size budget is a prompt to look for a split, not
a rule to cut blindly.

## Ways to split that keep each piece coherent

- **Refactor, then change.** A no-behaviour-change pull request first (the
  existing tests prove it), then a small one that changes behaviour.
- **Expand → migrate → contract.** Add the new column, kind or API alongside
  the old, move callers over, then remove the old. The standard shape for
  migrations and wire formats.
- **Seam first.** Land an interface with one implementation and its tests;
  add the others after.
- **Behind a flag.** Land pieces switched off, then flip the flag in a final
  small pull request.
- **Mechanical changes alone.** Renames, moves, formatting and regenerated
  files get their own pull request; it reviews in a glance.
- **Thin vertical slices over horizontal layers.** A minimal end-to-end path
  is coherent; "all the database changes" usually is not, unless it is a real
  expand step.

## Stacking

Prefer a **merge train** to a stack: open each pull request against `main`,
let it merge, then open the next. An approved low-risk pull request merges
within minutes, so sequencing is cheap, and a stack's children strand when a
parent lands late or its branch is deleted — `gh pr merge --delete-branch` on
a parent closes a child based on that branch, and a closed child cannot be
retargeted. If you must stack, keep it two deep and retarget the child to
`main` before merging the parent.

## When it has to stay whole

Some changes are genuinely atomic: both sides of a wire format, a migration
and the code that cannot run without it. Add to the description:

```markdown
## Why not split

<One or two sentences: what breaks if these land separately.>
```

Paste the section itself, not the fence around it, and replace the
placeholder. The check counts only prose you wrote: text inside code fences or
backticks, HTML comments, `<placeholders>` and link targets are removed before
it looks for the heading and measures the section, so the template pasted
unedited reads as empty and still fails. Link text and a sentence next to a
snippet do count. The check re-runs when you edit the description.

## Fork pull requests

GitHub gives a fork-originated `pull_request` run a read-only token, so the
check cannot label or comment on a pull request opened from a fork. It still
sizes it: the tier and the full breakdown go to the job summary and the log,
and an unjustified `size/XL` still fails, so enforcement is identical. The
only difference is that no `size/*` label appears and no comment is posted;
the step logs a notice saying so.

## Trust

The job checks out the base commit and runs `main`'s copy of
`.github/scripts/pr-size.js` and `.github/pr-size.json`, so a pull request
cannot change the rules it is judged by through those two files. It cannot
stop a pull request that edits `.github/workflows/pr-size.yml` itself, because
`pull_request` workflows run from the pull request; workflow files are
high-risk in the auto-merge table for that reason and always get a human
merge. Tests: `node --test .github/scripts/pr-size.test.js`.
