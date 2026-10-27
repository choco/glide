---
name: prepare-glide-release
description: Prepare Glide's next release by finding and checking out the open release-please pull request, reviewing the changes since the previous version, and rewriting the generated CHANGELOG section as accurate, user-friendly release notes. Use when asked to prepare a Glide release, polish release notes, update the release-please PR, or make the next version's changelog readable.
---

# Prepare Glide Release

Turn release-please's generated changelog into concise release notes that explain what users gain and how to use it.

## Workflow

1. Read the repository instructions and inspect the worktree. Preserve unrelated changes and untracked files.
2. Find the open release-please PR targeting `main`. Prefer a head branch matching `release-please--branches--main--components--glide-wm`, then check it out locally.
3. Identify the previous release tag and review every commit in the release range. Inspect relevant documentation, default configuration, and implementation details to verify behavior, command names, and default shortcuts.
4. Rewrite only the newest section of `CHANGELOG.md`:
   - Start with a brief overview of the release's main user benefits.
   - Deduplicate merge commits and their underlying commits into one entry.
   - Organize entries under only the headings that help readers, such as `Features`, `Bug Fixes`, or `Improvements`.
   - Describe observable user impact rather than implementation details.
   - Include commands, configuration, or default shortcuts when they help users adopt a feature.
   - Link each entry to the most useful PR, issue, or commit.
   - Omit refactors, dependency bumps, CI work, and other internal changes unless they affect users.
   - Do not claim behavior that the diff, documentation, tests, or commit history does not support.
5. Review the finished section against the release range to ensure no user-facing change was missed or counted twice.
6. Run `git diff --check` and report the checked-out PR, changed file, and verification performed. For a changelog-only edit, do not run the full test suite unless repository instructions require it.

Do not commit, push, merge, or publish the release unless the user explicitly asks for that action.
