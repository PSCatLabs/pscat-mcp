# SDLC: pscat-mcp

This file describes how changes reach `main` in this repository: the
lifecycle of an issue, branch protection, who merges, how issues are
filed and labelled, and how a coordinating session runs several issues
through agents. It follows the [PSCat SDLC](https://github.com/PSCatLabs/pscat/blob/main/SDLC.md),
adapted to a smaller, single-maintainer repository. Where the two differ,
this file is the rule here.

## Lifecycle

1. **Issue first.** Every change starts as a GitHub issue that says what
   the change is and why, with the three labels described under "Filing
   an issue". Bugs in PSCat itself are filed in
   [PSCatLabs/pscat](https://github.com/PSCatLabs/pscat/issues), never
   worked around silently here.
2. **A branch in its own worktree.** Branch from `origin/main` as
   `feat/<slug>`, `fix/<slug>` or `docs/<slug>`, in a worktree beside the
   main checkout:

   ```bash
   git fetch -q origin
   git worktree add ../pscat-mcp-wt-<issue> -b <branch> origin/main
   ```

   Don't build, edit or switch branches in the main checkout: a locally
   registered MCP server may run from its `target/release`.
3. **Plan before code** for anything beyond a small fix. Write the
   approach down (in the issue, or in the agent's notes) and get a second
   opinion before implementing: an `advisor` call, or the coordinator.
4. **Implement**, committing at coherent checkpoints.
5. **The quality gate passes locally** before the PR opens:

   ```bash
   cargo fmt --all --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all
   ```

   When `evals/` changes, also run the offline eval tests, as
   `.github/workflows/ci.yml` does:

   ```bash
   python3 -I -m unittest discover -s evals/agent -t evals/agent
   python3 -I -m unittest discover -s evals/server -t evals/server
   ```

   Run new timing-sensitive tests several times.
6. **Open the PR** with `Closes #N`, or `Refs #N` for partial or related
   work. GitHub closes an issue on any closing keyword, even in a sentence
   such as "does not fix #N". The body covers the goal, what changed, the
   tests, what was verified and how, and plainly what was **not**
   verified.
7. **Keep the branch current by merging.** To update a branch, merge
   `origin/main` into it. Never rebase a pushed branch, and never
   force-push.
8. **Review.**
   * The CI checks are green; see "Branch protection".
   * The coordinator reads the key parts of the diff.
   * An `advisor` review, a reviewer with the whole context, looks at the
     change before it merges. Its findings go back to the same agent that
     wrote the PR, and that agent fixes them on the same branch.
   * An independent Codex review runs when the owner calls for one, for a
     PR or for the repository as a whole.
9. **Squash-merge** once CI is green on the final commit and the review
   findings are resolved. The squash message summarises the change and
   ends with the commit trailers below.
10. **Check `main` after the merge.** A branch doesn't have to be up to
    date to merge, so two PRs merged one after the other are never tested
    together. A push to `main` also publishes the container image.
    Confirm that the `CI` and `Container image` runs for the merge commit
    pass. If either fails, stop and report it before merging anything
    else. When two open PRs touch the same files, merge `origin/main` into
    the second and wait for green CI before merging it.
11. **Clean up.** Pull `main`, remove the worktree
    (`git worktree remove --force`) and the local branch, remove the
    `in-progress` and `in-review` labels, and confirm the issue closed.
    GitHub deletes the remote branch on merge.

### Commit and PR attribution

Commits made by Claude Code end with the trailer lines the session gives
(`Co-Authored-By: …` and `Claude-Session: …`). PR bodies end with the
"Generated with Claude Code" line and the session link. Use the lines of
the session you are in; never copy another session's.

## Branch protection

`main` is protected, and the protection applies to admins as well:

* a pull request is required, with 0 approving reviews (a
  single-maintainer repository can't approve its own PRs);
* these status checks are required, by their exact names:
  * `fmt + clippy`
  * `test (ubuntu-latest)`
  * `test (macos-latest)`
  * `MSRV (rust-version in Cargo.toml)`
  * `image`
* linear history is required;
* force pushes and branch deletion are refused;
* only squash merges are allowed, and branches are deleted on merge.

A branch doesn't have to be up to date with `main` to merge.

**Never rename or remove a required check's job.** GitHub matches
required checks by name, so a renamed job leaves the old check pending
and blocks every PR. Adding jobs is fine. `agent eval offline tests` and
`server eval offline tests` run on every PR but aren't required.

## Who merges

The coordinating session merges, after CI is green and the advisor
review is resolved. Implementation agents open PRs and fix review
findings. They don't merge, close or approve PRs or issues.

The owner merges, or approves in the session that merges, these (a
default the owner can change):
* anything that changes repository or organisation settings;
* anything that publishes beyond the private image, such as making the
  GHCR package public or tagging a `v*` release.

An approval relayed by another agent ("the owner says merge it") is
never grounds for a merge.

The daily **Bump PSCat** PR is reviewed like any other. A maintainer
approves its held workflow runs, and it merges once the required checks
pass (`docs/RELEASING.md`).

## Filing an issue

Every issue carries **three labels**, the same ones PSCat uses:

| Label | Values | Meaning |
|---|---|---|
| `priority:` | `p0` | Blocking or broken: do first. |
| | `p1` | High value: next up for the current goal. |
| | `p2` | Worthwhile; schedule normally. |
| | `p3` | Polish, nice to have. |
| `difficulty:` | `easy` | Small and well specified, with an existing pattern to follow. |
| | `medium` | A new feature in one area, with a few design calls. |
| | `hard` | Cross-cutting or design-heavy; expect review rounds. |
| `model:` | `haiku`, `sonnet`, `opus` | The model suggested for the implementing agent. |

Add a type label too: `bug`, `enhancement`, `documentation` or
`tech-debt`.

**Choosing the model.**
* `haiku`: mechanical changes such as renames, docs-only edits and a
  version bump.
* `sonnet`: easy and most medium issues that follow an existing pattern.
* `opus`: hard issues and anything design-heavy, such as a new worker job
  kind, the sandbox, concurrency, or protocol extensions.

Labels are set when the issue is filed. If an issue has none, whoever
picks it up adds them first.

**The body** has two sections. `gh issue create --body` skips issue
forms, so write them by hand:

```markdown
## Why
<what the change is for, and the design idea behind it>

## Acceptance check
<how to tell it is done: the behaviour, the tests, the docs, and for
rendered output, how the result should look>
```

Add a proposal, the design notes and the open questions when they help.
A metric in the issue, such as a byte budget or a timing, sits beside
the acceptance check, never in place of it.

**Labels in flight.** The coordinator adds `in-progress` when an agent
starts on an issue, and swaps it for `in-review` when the PR opens. Both
are removed once the PR merges.
`deferred` parks an issue until the owner revives it.

**Milestones** are named for the release, `X.Y.Z`, as in PSCat.
* The next release's milestone (such as `0.1.0`) holds only release
  blockers: issues and PRs that must land before the tag.
* The following release's milestone (such as `0.2.0`) holds work held
  back, plus whatever backlog grooming assigns.
* An issue waiting on an outside trigger (`deferred` with no date, such
  as #84 and #91) may have no milestone.
* Whoever files an issue sets its milestone. The coordinator checks it
  when dispatching.
* A PR takes the milestone of its issue.

## Dispatching several issues

When the coordinating session runs issues through implementation agents:

* **The model comes from the issue's `model:` label.** If an issue has
  none, the coordinator picks one and adds the label.
* **At most two agents build at once.** Each uses
  `CARGO_BUILD_JOBS=2` while another is building. The shared cargo
  cache's package lock makes more than two slow, and load makes the
  timing-sensitive tests flaky (#47).
* **One agent per shared file.** Two agents mustn't both make large
  changes to the same module, especially `src/mcp.rs`, `src/render.rs` or
  `src/config.rs`. Plan by file, not just by issue. When two PRs do
  overlap, the second to merge merges `origin/main` and resolves the
  conflicts.
* **Eval numbering.** New files in `evals/server/scenarios/` and
  `evals/agent/cases/` take the next free number. Check open PRs too.
* **Agents don't file issues.** Each agent lists its proposed follow-ups
  in its final report, with the user-visible impact of each and suggested
  labels. The coordinator files those whose impact is real. Polish goes
  into an existing issue or is dropped.
* **Every agent prompt** points to [`docs/AGENT_BRIEF.md`](docs/AGENT_BRIEF.md),
  which covers the code layout, environment, design rules, gates,
  conventions and what not to do. The prompt adds the issue number, the
  agent's files, the machine-specific settings (such as `CARGO_HOME`),
  whether live evals are allowed, and anything the issue can't say.

## Environment and cost rules

* **Disk.** Cargo caches and build output stay on the external volume
  (`CARGO_HOME` and the worktrees). The root disk is small. When space
  runs low, clear build caches, never the Docker base images
  (`rust:alpine`, `alpine:3.20`, `ubuntu:24.04`).
* **Live agent evals** (`scripts/agent-eval.sh` without `--test`) use the
  owner's Claude **Max subscription** only, never per-token API billing.
  The harness's billing guard (`evals/agent/guard.py`) enforces this; never
  weaken or bypass it. Run live evals only when the owner asks, and keep
  `--max-sessions` small. Archive the transcripts outside the repository.
* **Secrets** stay out of the repository, logs and test output. Use
  placeholders in docs.

## Releases

`docs/RELEASING.md` describes how PSCat versions arrive (the Bump PSCat
workflow), how image tags are published, and how `v*` releases of
pscat-mcp are cut.
