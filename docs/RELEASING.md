# Releasing

pscat-mcp ships a release after every PSCat release. A release contains:

- one PSCat version, chosen by the `tag` in `Cargo.toml`;
- one server version, from `[package] version` in `Cargo.toml`;
- one container image `ghcr.io/pscatlabs/pscat-mcp:<version>`.

## Where the PSCat version comes from

```toml
pscat = { git = "https://github.com/PSCatLabs/pscat", tag = "v0.7.0-rc.4", default-features = false }
```

This line is the only place the version is chosen.

- `Cargo.lock` records the commit the tag resolved to.
- Every build in CI and in the Dockerfile uses `--locked`, so a build can't silently move to a different commit.
- `build.rs` reads the PSCat version and commit from `Cargo.lock`. They show up in `get_renderer_info`, in every tool result's `renderer` field, and as the image tag `pscat-<version>`.
- A test checks that these values match the lockfile.

There is deliberately no build argument or environment variable that overrides the PSCat version at build time. One would bypass the lockfile, and an image could then report one PSCat version while containing another.

## Release steps

1. **Bump PSCat.** Use one of these:
   - Wait for the daily **Bump PSCat** workflow. It picks up the newest PSCat tag, pre-releases included.
   - Run it now from the Actions tab or with `gh workflow run bump-pscat.yml -f tag=v0.7.0 -f release_version=0.2.0`. Both inputs are optional: with no `tag` it uses the newest tag, and with no `release_version` the server's version stays as it is.
   - Locally: `scripts/bump-pscat.sh v0.7.0 --release-version 0.2.0 --test`. Then commit on a branch and open a pull request.

   The daily run follows PSCat **pre-release** tags too (`v0.7.0-rc.N`), and it doesn't change the server's version. Before merging a scheduled bump that you want to release, set the version: re-run the workflow with `release_version`, or push a commit to the bump branch with `scripts/bump-pscat.sh --latest --release-version X.Y.Z`. The tag check in step 3 rejects a release whose tag doesn't match `Cargo.toml`.

   The workflow runs `cargo test --all --locked`, pushes `bump/pscat-<tag>[-release-<version>]` and opens the pull request as `github-actions[bot]`.

   **Approve the checks.** GitHub holds the workflows of a pull request opened by a workflow. Open the pull request and click **Approve workflows to run**. The five required checks then run: fmt + clippy, tests on Linux and macOS, MSRV, and the amd64 image with the smoke test.

   The workflow can open pull requests only while *Allow GitHub Actions to create and approve pull requests* is on, both in the organization's and the repository's *Settings → Actions → General*. If either is off, it pushes the branch and puts an "Open the pull request" link in the run summary. A pull request you open from that link runs its checks without approval.

2. **Review and merge** once CI and the container smoke test are green. `main` is protected:
   - every change goes through a pull request, which can be merged without an approving review;
   - those five checks must pass, for admins too;
   - merges are squash-only, with linear history;
   - no force-pushes or deletion. PSCat release notes may change rendering. Check the README examples, or `examples/*.ps`, against the new version if anything looks different.

3. **Tag the release** from the merged `main`:

   ```bash
   gh release create v0.2.0 --target main --generate-notes \
     --notes "Built with PSCat v0.7.0."
   ```

   Pushing the `v0.2.0` tag runs the **Container image** workflow. It fails at once unless the tag equals `v` + the `Cargo.toml` version. Otherwise it builds the image, runs the smoke test against it, and publishes these tags:
   - `0.2.0`
   - `pscat-0.7.0`
   - `sha-<commit>`

4. **Deploy** by tag or digest. See [CLOUD_RUN.md](CLOUD_RUN.md).

## When PSCat changes its library API

The bump compiles against the new PSCat, so an API change shows up as a failed **Bump PSCat** run. pscat-mcp still uses two PSCat modules that are not part of its documented API. Check these first:

- `pscat::lexer`, in `src/worker.rs` (`syntax_scan`). Tracked in PSCatLabs/pscat#1096.
- `pscat::gfx::PageSizeSource`, in `src/worker.rs` (`configure`). Tracked in PSCatLabs/pscat#1097.

When those issues land, switch to the public APIs.
