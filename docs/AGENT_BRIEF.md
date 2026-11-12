# Brief for implementation agents

Every agent that implements an issue in this repository reads this first.
It goes with [`SDLC.md`](../SDLC.md), which covers the process; this
file covers the code and the environment. The coordinator's prompt adds
the issue number, the files that are yours, and anything the issue can't
say.

Read the issue first (`gh issue view N -R PSCatLabs/pscat-mcp`) and stay
inside its scope.

## Orientation

Read the [README](../README.md) and [`docs/ARCHITECTURE.md`](ARCHITECTURE.md)
first. The main code:

* `src/mcp.rs`: the tool router, handlers, and the `call_tool` wrapper;
* `src/render.rs`: `RenderService`, limits, slots and async jobs (with
  `src/jobs.rs`);
* `src/worker.rs`: the `render-worker` child process and its job
  protocol;
* `src/generate.rs`, `src/sheet.rs`: generator jobs (`chalkboard_math`,
  `handwrite`, `review_sheet`);
* `src/http.rs`, `src/auth.rs`: the HTTP transport and bearer auth;
* `src/guide.rs`: MCP prompts and resources from `guide/`;
* `src/pscat_extras.rs`: every call into PSCat's `#[doc(hidden)]` APIs.

Tests live in `tests/`, with one file per area, and use the real PSCat
worker. The README's Tests table lists them.

**Parity source.** PSCat's own plugin MCP server is `src/mcp_server.rs`
in the **pinned** PSCat checkout: the revision in `Cargo.lock`. Find it
with `ls -d "$CARGO_HOME"/git/checkouts/pscat-*/<first 7 of the commit>`.
Don't use a newer local clone of PSCat.

* Hidden modules (`#[doc(hidden)]` in PSCat's `src/lib.rs`) are usable,
  but only through `src/pscat_extras.rs`. Each use is tracked in
  PSCatLabs/pscat#1098.
* Check whether a module needs a cargo feature that
  `default-features = false` turns off.

## Environment

* **Work only in your own worktree** (see `SDLC.md`, step 2). Never
  build, switch branches or edit in the main checkout: the owner's
  locally registered MCP server runs from its `target/release`.
* **Keep cargo's cache and build output off a small system disk.** Set
  `CARGO_HOME` as the coordinator's prompt says, and keep the worktree
  beside the main checkout. Use `CARGO_BUILD_JOBS=4`, or 2 when another
  agent is building.
* **MSRV is the `rust-version` in `Cargo.toml`.** Use no std or
  dependency API newer than that, even if your local rustc is newer.

## Design rules

* **Untrusted input runs only in the worker process.** That covers
  PostScript, LaTeX, note text and sheet entries, which run under the
  worker's rlimits and deadline. Never process them in the server: a
  stack overflow or a hang there takes down every user. The server may
  do bounded arithmetic on arguments (a `plan` step); nothing that
  parses or runs them.
* **Inline output only.** No filesystem writes and no `out_path`.
* **Errors use the structured envelope** (`structuredContent.error.code`)
  with the existing error codes where they fit. `error.input.code` names
  the budget or input that failed.
* **Tool descriptions are short and neutral**: what the tool does, its
  arguments and its limits. Never instruct the agent ("always use",
  "prefer").
* **Every limit is configurable** like the existing ones: a CLI flag
  plus a `PSCAT_MCP_*` variable, with a sensible default. Document it in
  the README configuration table and `.env.example`.
* **Keep the docs accurate** for what you change: the README (tools
  table, limitations, tests table), `examples/agent-scenarios.md` and
  `docs/ARCHITECTURE.md`.

### Tool results and what the model sees

* **Keep every tool response small enough for an agent's context**,
  about 24 KB at most, counting `content` and `structuredContent` alike.
  Put large data behind optional filter or pagination arguments.
* **Text and `structuredContent` carry the same information.** Claude
  Code gives the model a tool's `structuredContent` JSON *instead of* its
  text, while image blocks still go through separately. So
  `structuredContent` must stand on its own and stay small.
* **Embedded resources (SVG, PDF)** reach the model as text or as a
  note that the client saved a file.

### Adding a tool

* Put any new `#[doc(hidden)]` PSCat call in `src/pscat_extras.rs`.
* Add a row to `EXPECTED_TOOLS` in `tests/mcp_stdio.rs`.
* A tool with arguments needs an arm in `with_argument_error_envelope`
  in `src/mcp.rs`; without it, its argument errors lose the structured
  envelope.
* Add a server-layer scenario under `evals/server/scenarios/` and, where
  it makes sense, an agent case under `evals/agent/cases/`. Each takes
  the next free number; check open PRs too. New agent cases stay
  unbaselined until the owner runs a live baseline.

## Evals

* `evals/server/`: server-layer scenarios, run by `cargo test` over
  stdio and HTTP, and against the container image in CI.
* `evals/agent/`: the agent-layer harness, in standard-library Python.
  Its offline tests run with
  `python3 -I -m unittest discover -s evals/agent -t evals/agent`.
* **Never run live agent evals** (`scripts/agent-eval.sh` without
  `--test`) unless your prompt explicitly says so. They spend the
  owner's subscription. Never weaken `evals/agent/guard.py`.

## Gates before the PR

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

All three must pass locally. Run new timing-sensitive tests several
times.

## Git and PR

* **One branch, one PR.** Use `Closes #N`, or `Refs #N` for partial
  work.
* **Commit and PR trailers:** use the attribution lines your own session
  gives you. On commits, the `Co-Authored-By` line names the model you
  are actually running.
* **The PR body** covers the goal, what changed, the tests, what you
  verified and how, and plainly what you did **not** verify.
* **To update the branch, merge `origin/main`.** Never rebase, never
  force-push.
* **Push and open the PR:**
  1. `git push -u origin <branch>`
  2. `gh pr create`
  3. `gh pr checks <N> --watch --interval 30`

  Fix failures and push again.

## Don'ts

* Don't merge, close or approve PRs or issues. The coordinator does
  that.
* Don't file issues. List proposed follow-ups in your final report, each
  with its user-visible impact and suggested `priority:`, `difficulty:`
  and `model:` labels.
* Don't rename or remove CI jobs (see `SDLC.md`, "Branch protection").
  Adding a job is fine.
* Don't change MCP client registrations or their config files
  (`claude mcp add`/`remove`, `codex mcp add`/`remove`, `~/.claude.json`,
  `~/.codex/config.toml`).
* Don't delete Docker images, and don't change repository or
  organisation settings.
* Don't remove your worktree; the coordinator does that after merging.
* Don't set milestones on issues or PRs; the coordinator does that (see `SDLC.md`, "Milestones").

## Your final report

* The PR URL and the final CI status of each check.
* A summary of the design.
* What you verified, and how.
* Where you deviated from the issue, and why.
* Proposed follow-ups, with their labels.
* Open questions.
