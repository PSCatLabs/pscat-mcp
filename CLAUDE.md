# pscat-mcp: notes for agents

* **Process:** read [`SDLC.md`](SDLC.md) before starting work. It covers
  the issue, worktree branch, PR, review and squash-merge lifecycle, the
  required CI checks, issue labels (`priority:`, `difficulty:`, `model:`)
  and the dispatching rules.
* **Code and design:** [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md),
  then the README's configuration, security model and limitations
  sections.
* **Releases and PSCat versions:** [`docs/RELEASING.md`](docs/RELEASING.md).
* **Testing with real clients:** [`docs/LOCAL_TESTING.md`](docs/LOCAL_TESTING.md).

Rules that are easy to miss:

* Untrusted input (PostScript, LaTeX, note text, sheet entries) is
  processed only in the worker process, never in the server.
* Never rename a required CI job. To update a branch, merge `origin/main`
  into it; never rebase, never force-push.
* Live agent evals run only on the owner's Max subscription, through
  `evals/agent/guard.py`, and only when the owner asks.
