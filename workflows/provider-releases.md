# Provider release review

Run these steps to review the stable releases an agent has shipped since its
last review. Any coding agent or person can run them. The record lives in
[PROVIDERS.md](../PROVIDERS.md#upstream-releases): the `## Upstream releases`
table, the definitions under it, and each agent's
`### Upstream findings: <agent>` subsection.

Each agent has a file in [`provider-releases/`](provider-releases/), named for
its `labels().name` (`codex.md` for Codex). The file holds what differs by
agent: the release listing command, the source repository and its clone
command, the storage paths to diff, and how to read any schema version
rearview checks. An agent released anywhere other than GitHub gets its own
listing command there.

A review reads release notes, source and session files. It does not run the
agent.

## Steps

1. **List new releases.** Run the listing command in the agent's file. It
   prints bare stable versions, newest first. Keep the versions newer than
   "Reviewed through". When even the oldest version printed is newer than
   "Reviewed through", the list was cut short: raise `--limit` and run it
   again. An agent with no new release is done. For an agent not reviewed yet
   (`—`), review only the newest stable release: read its storage paths in
   full, not as a diff, against PROVIDERS.md's tables, then update the row as
   usual.
2. **Check for a clone.** When the agent's file names a source repository and
   no local clone of it exists, stop: the review's result is "not cloned",
   with the clone command from the agent's file. An agent with no published
   source goes to step 4 and reads its release notes only.
3. **Fetch tags** into the clone from the agent's repository:

   ```sh
   git fetch --tags <remote>
   ```

   `<remote>` is `origin` in a fresh clone, or the remote that `git remote -v`
   shows as the agent's own repository in a clone of a fork.

4. **Read each new release, oldest first.** Read its release notes. In long
   release notes, search for storage words first (`rollout`, `history`,
   `migration`, `session`, `thread`), then read the matching entries. Diff the
   storage paths from the agent's file between the previous release's tag and
   this release's tag, without checking anything out:

   ```sh
   git diff <previous-tag> <tag> -- <storage paths>
   ```

5. **Report what rearview drops from local sessions.** Run the hidden report:

   ```sh
   cargo run --release -- --report-dropped-records
   ```

   It prints the record kinds each reader drops, per agent and the version
   each record falls under, with counts and an outcome: skipped,
   unrecognised or failed. A cold run reads every session and takes minutes.
   Find each `unrecognised` or `failed` kind under a version this review
   covers in that release's notes and diff. For an agent with no published
   source, this is the main check.
6. **Compare each change with what rearview reads:** the agent's Storage and
   Transcript format tables in PROVIDERS.md, and the schema version the agent's
   file describes.
7. **Record each finding** in the agent's findings subsection, with the format
   and a status from PROVIDERS.md. Its version is the release that introduced
   the change. A schema version past the one rearview checks is one
   `needs evaluation` finding per release that moved it, until someone reads
   whether those migrations touch the tables rearview queries.
8. **Revisit every deferred finding.** A deferred change that is now on by
   default becomes `pending support`.
9. **Settle judged findings.** In the review that judges a `needs evaluation`
   finding, delete it when rearview needs no change, or make it
   `pending support` when rearview does. Settling every schema finding moves
   the schema version rearview checks, as the agent's file describes.
10. **Update the row.** Set "Reviewed through" to the newest stable release
    read. Set "Accounted for through" to the stable release before the
    earliest open finding, or to "Reviewed through" when no finding is open.
    Set "Last review" to today's date.
11. **Resolve a finding** in the change that adds its support: delete its
    bullet, then set "Accounted for through" and "Last review" as in step 10.
12. **Run** `cargo test upstream_releases`, then `just check`, or
    `checkle run all` where bash is unavailable.
13. **Skip the changelog for a review.** The change that resolves a finding
    gets the usual entry for what the user now sees.
