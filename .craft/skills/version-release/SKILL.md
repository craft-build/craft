---
name: version-release
description: Cut a new release by reviewing changes since the last version tag, bumping the Cargo version, updating the changelog, and committing in conventional commit format
when_to_use: When the user asks to create a release, cut a version, bump the version, or prepare a new tagged release of the project
---

# Version Release Workflow

Creates a release commit: review changes since the last tag, let the user pick the next version, bump Cargo version, update deps, write the changelog entry, and commit.

## Steps

1. **Find the last version tag** and list the changes since it:
   ```sh
   LAST_TAG=$(git describe --tags --abbrev=0 2>/dev/null)
   if [ -n "$LAST_TAG" ]; then git log --oneline "$LAST_TAG"..HEAD; else git log --oneline; fi
   ```
   Read the commit subjects to determine what changed (feat / fix / breaking). If uncertain, skim a few commit diffs. If no tag exists yet, treat the whole history as the change set and this will cut the first release.

2. **Determine the current version.** For a workspace, read `version` (or `workspace.package.version`) from the root `Cargo.toml`. Check whether member crates pin their own versions.

3. **Ask the user which version to release using the `question` tool.** Offer options based on the actual changes:
   - Patch (x.y.Z) if only fixes and chores
   - Minor (x.Y.0) if new features, no breaking changes (Recommended default when features exist)
   - Major (X.0.0) if breaking changes
   - For a first release, offer the current version as-is alongside 1.0.0
   Include the concrete computed version numbers in each option label (e.g. `Minor — 0.12.0 (Recommended)`). Do not guess the answer; wait for the user's selection.

4. **Bump the version** in `Cargo.toml` (root `[workspace.package]` `version`, or each crate that tracks its own version). Use targeted edits, keep formatting intact.

5. **Update dependencies:**
   ```sh
   cargo update
   ```
   This also refreshes the workspace version in `Cargo.lock`. If it fails, report the error and stop; do not hand-edit the lockfile to repair it.

6. **Add a CHANGELOG.md entry.** Read the top of `CHANGELOG.md` first and match its existing format (heading style, category names like Added/Changed/Fixed, line style). Prepend a new entry for the chosen version with today's date, grouped into categories derived from the commits found in step 1. If `CHANGELOG.md` does not exist, create it with a `## [<version>] - YYYY-MM-DD` section.

7. **Verify the project still builds and passes checks before committing:**
   ```sh
   cargo clippy --all-features --all --tests -- -D warnings
   cargo nextest run --all-features --workspace
   ```

8. **Create the release commit** with a conventional commit message:
   ```sh
   git add Cargo.toml Cargo.lock CHANGELOG.md
   git commit -m "chore(release): v<version>"
   ```
   Stage only release-related files. Do not sweep unrelated working-tree changes into this commit.

9. **Report the result** to the user: new version, commit hash, and that the tag itself was NOT created unless they explicitly asked for it (`git tag v<version>`).
