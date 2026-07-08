# Releasing

The version bump rides the final feature PR of a release cycle — there is no
bump-only PR. The release workflow enforces this: reused assets must embed the
tag's exact version, so a tag pointing at a tree that built a different
version fails loudly.

## The cycle

1. Land feature PRs on `main` as usual.
2. The **last** PR of the cycle also bumps the version in three places:
   - `Cargo.toml` (workspace package version)
   - `Cargo.lock` (via `cargo update -p visible-browser-lab` or any build)
   - `.codex-plugin/plugin.json`
3. Merge it. That PR's dry-run built and packaged the release assets already.
4. Tag the merge commit and push the tag:

   ```sh
   git tag -a v0.X.Y -m "v0.X.Y: summary" <merge-commit>
   git push origin v0.X.Y
   ```

5. The Release workflow's `reuse` job finds the successful PR dry-run whose
   head tree matches the tagged commit's tree (annotated tags are peeled),
   verifies a successful CI run on that commit, re-verifies `SHA256SUMS`,
   checks every asset embeds the tag's version, and republishes the dry-run's
   artifacts. Tests, build, and package all skip. Measured: v0.4.5 went
   tag-to-release in 21 seconds (v0.4.4, pre-fast-path, took 24 minutes).

6. Verify and install:

   ```sh
   gh release download v0.X.Y -p 'SHA256SUMS' -p '*darwin-arm64*'
   shasum -a 256 -c SHA256SUMS --ignore-missing
   code --install-extension visible-browser-lab-vscode-*.vsix
   ```

   The running broker self-displaces on version mismatch (RFC 00007) — no
   manual process management needed.

## When the fast path doesn't fire

If no matching dry-run artifact exists (tree changed after the PR, artifact
expired, or the tag points somewhere that never had a PR), the full pipeline
runs instead: tests, six-target build matrix, package, publish. Slower but
always correct. The fast path is an optimization, never a requirement.
