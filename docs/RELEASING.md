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
- `scripts/vendor-guide.py` copies the PSCat plugin's skills (`plugin/skills/`) from that commit into `guide/`, and `guide/SOURCE.json` records the tag and commit. The server serves them as MCP resources and prompts. `tests/guide.rs` fails if `guide/` differs from the pinned checkout, or if `SOURCE.json` names another commit than `Cargo.lock`.

There is deliberately no build argument or environment variable that overrides the PSCat version at build time. One would bypass the lockfile, and an image could then report one PSCat version while containing another.

## Release steps

1. **Bump PSCat.** Use one of these:
   - Wait for the daily **Bump PSCat** workflow. It picks up the newest PSCat tag, pre-releases included.
   - Run it now from the Actions tab or with `gh workflow run bump-pscat.yml -f tag=v0.7.0 -f release_version=0.2.0`. Both inputs are optional: with no `tag` it uses the newest tag, and with no `release_version` the server's version stays as it is.
   - Locally: `scripts/bump-pscat.sh v0.7.0 --release-version 0.2.0 --test`. Then commit on a branch and open a pull request.

   The daily run follows PSCat **pre-release** tags too (`v0.7.0-rc.N`), and it doesn't change the server's version. Before merging a scheduled bump that you want to release, set the version: re-run the workflow with `release_version`, or push a commit to the bump branch with `scripts/bump-pscat.sh --latest --release-version X.Y.Z`. The tag check in step 3 rejects a release whose tag doesn't match `Cargo.toml`.

   `bump-pscat.sh` also refreshes `guide/` from the new pin. PSCat may have changed the skills' text, so look over the `guide/` diff. The server's note on each guide (`src/guide.rs`) is built from the tool list, so it needs no edit unless a skill now describes something this server handles differently; `FILE_NOTES` holds per-file lines.

   The workflow runs `cargo test --all --locked`, commits `Cargo.toml`, `Cargo.lock` and `guide/` (with `git add -A`, so new and removed guide files are included), pushes `bump/pscat-<tag>[-release-<version>]` and opens the pull request as `github-actions[bot]`.

   **Approve the checks.** GitHub holds the workflows of a pull request opened by a workflow. Open the pull request and approve the workflows awaiting approval, using the banner and button near the merge box. The five required checks then run: fmt + clippy, tests on Linux and macOS, MSRV, and the amd64 image with the smoke test.

   The same setting also lets workflows *approve* pull requests. That is harmless while `main` requires no approving reviews. If you ever require one, a workflow could satisfy it, so revisit the setting then.

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

The bump compiles against the new PSCat, so an API change shows up as a failed **Bump PSCat** run. pscat-mcp uses several PSCat modules that are not part of its documented API (`lexer`, `gfx`, `halftone`, `png`, `chalk`, `pdf`, `capabilities`, `contact_sheet`). Every call to them is in `src/pscat_extras.rs`, whose module documentation lists them; check that file first. They are tracked in PSCatLabs/pscat#1098 (and PSCatLabs/pscat#1096 for the lexer, PSCatLabs/pscat#1097 for `PageSizeSource`). When those issues land, switch to the public APIs.

**tiny-skia.** `src/pscat_extras.rs` names PSCat's raster type, `tiny_skia::Pixmap`, for the review sheet. PSCat does not re-export tiny-skia, so pscat-mcp depends on it directly, at the version PSCat uses (`tiny-skia = "0.12"` in `Cargo.toml`). A Bump PSCat pull request must keep the two on the same version: if PSCat moves to another tiny-skia, the build fails with mismatched `Pixmap` types, and `cargo tree -d` lists two copies of `tiny-skia`. Change the version in `Cargo.toml` to PSCat's and check that `cargo tree -d` lists one.
