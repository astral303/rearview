# Development

## Setup and checks

rearview uses [mise](https://mise.jdx.dev) to install its development tools
and to run its tasks. Install mise first; it provides everything else. CI
runs the same tasks, so `mise run check` on your machine runs the check CI
runs.

- `mise.toml` specifies each tool and its version, including the Rust
  toolchain.
- `mise.lock` records each tool's download URL and checksum for every
  platform.

Then install the tools and run the checks:

```sh
mise install
mise run check
```

`mise run check` runs these tasks in parallel and does not rewrite files. Run
one alone with `mise run <task>`.

| Task                  | Fails when                                                                           |
|-----------------------|--------------------------------------------------------------------------------------|
| `check-toolchain-pin` | `rust-toolchain.toml` and `mise.lock` specify a different Rust version or components |
| `check-scripts`       | a bash task or script has a syntax error                                             |
| `format-check`        | Rust code is not formatted                                                           |
| `clippy`              | Clippy reports any warning                                                           |
| `test`                | a test fails                                                                         |
| `build`               | a package does not build                                                             |

## Versions and dependencies

rearview is an application, not a library. Every version change is a
deliberate edit that a reviewer sees.

- Specify an exact version for every dependency, tool, GitHub Action and CI
  runner: never a range, `latest` or a moving tag.
- Pin a GitHub Action to a commit, with its version in a comment.
- The lock files fix what `Cargo.toml` and `mise.toml` cannot: `Cargo.lock`
  holds the version of every transitive dependency, and `mise.lock` each
  tool's download and checksum. Never regenerate a lock file from scratch,
  for example by deleting it or running `cargo generate-lockfile`.
- Change a direct dependency or a tool in `Cargo.toml` or `mise.toml`: a
  one-line manifest change is easier to review than a lock file update.
  Change a transitive dependency alone, with
  `cargo update -p <crate> --precise <version>`.
- Bump a mise tool by editing its version in `mise.toml`, then run
  `mise lock <tool>` and `mise install`. `mise install <tool>` resolves the
  version again and ignores `mise.lock`.

Dependency policy:

- Do not update every dependency at once, as `cargo update` without a
  package does.
- Review every dependency update: read what changed in the dependency
  between the two versions.
- Do not trust semantic versioning. A patch or minor update gets the same
  review as a major one.

## Design principles

- **No surprises, no needless strictness.** rearview behaves as the user
  would expect from what they see. A rule stricter than they expect makes
  them fail at the task and then diagnose why; a rule looser than they
  expect gives them what they didn't ask for. Examples:
  - A UUID pasted in either case finds its session, because UUIDs are
    case-insensitive.
  - An OpenCode `ses_` ID matches exactly, because its case distinguishes
    one ID from another.
  - Every filter (the time range, the workspace, excluded projects) hides
    the same sessions from the list and from search results, so no row
    appears under a query and disappears when the query is cleared.
    Session-ID lookup is the one exception.
- **Everything an agent can find, the user can find, and the reverse.** The
  TUI and the `rearview agent` commands read one cache and run one search.
  Do not give the agent commands a store, index or search path of their
  own.
- **Fallback modes only on request.** Add a fallback mode only when the
  maintainers ask for it as product behaviour. Otherwise, new code that
  cannot do its job stops and reports why. Each fallback mode is a second
  code path to test and maintain.
- **Compute a fact once.** When discovery or a parse computes a fact about
  a session, such as its sub-agent transcripts, store it with the session
  (`SessionStub`, `ListedSessionEntry`, `Conversation`) and read it from
  there. Do not add a lookup that computes it again, and do not read or
  parse a transcript again to recover a fact an earlier step dropped.

## Tests

Unit tests are fast, isolated and repeatable. The target: the test suite runs
in under a minute on a slow Windows machine, not counting compilation. When
the suite's time jumps, find the test that caused it and make that test
cheaper.

Tests do not touch real conversations: they read fixture sessions from
temporary directories.

### Test types

| Type                   | Where                                              | Runs                                |
|------------------------|----------------------------------------------------|-------------------------------------|
| Unit tests             | `#[cfg(test)]` modules under `src/`                | `mise run test`                     |
| Integration tests      | `tests/`                                           | `mise run test`                     |
| Semantic model parity  | `src/semantic/fastembed/parity_record.rs`, ignored | by hand, after a fastembed upgrade  |
| Runtime library lookup | `mise run test-dynamic-ort`                        | CI                                  |
| Release binary check   | `mise run verify-semantic-search`                  | the release workflow, on each build |

- Unit tests are most of the suite. They are sociable: each tests behaviour
  through real pieces together, not through test doubles of its
  collaborators. The tests over every provider's fixtures are unit tests
  too.
- Integration tests run the built binary or check the repository:
  - `agent_cli.rs` runs the binary's `rearview agent` commands and checks
    their output. `tests/common` points every agent's session location and
    the cache at a temporary directory.
  - `shared_code_names_no_agent.rs` fails when shared code names an agent
    instead of reaching it through its provider.
  - `mise_tasks.rs` checks that each script in `mise-tasks/` is the command
    its task runs.
  - `cargo_manifest.rs` checks that semantic search is always built in.
- The parity test is the one test outside the everyday run. It downloads
  and loads the semantic model, and its result changes only when fastembed
  does. After a fastembed upgrade, run
  `cargo test --lib load_model_embeds_as_fastembeds_loader_does -- --ignored`
  to refresh the parity record.
- The Linux release build loads ONNX Runtime at run time from a bundled
  library (the `release-dynamic-ort` feature). `test-dynamic-ort` tests how
  that build finds the library.
- `verify-semantic-search` checks that a release binary loads the semantic
  model and computes embeddings for a seeded history.

### Test first, whenever reasonable

- A bug fix starts with a test that fails on the bug (red), then the fix
  makes it pass (green).
- A new feature or other change starts with its test when the new behaviour
  can be expressed as a test up front.
- While exploring an implementation direction, or writing something new
  without an obvious test structure, write the tests once the design
  settles. Tests written that early cost time and constrain a design that is
  still changing.

### Real pieces, test doubles at the edges

- Test behaviour through entry points such as a TUI `App` action,
  `load_sessions_with_cache` or an agent command, with real pieces: fixture
  transcripts and a session cache in temporary directories
  (`SessionCacheStore::under`), and a `RootEnvironment` the test builds.
- Use test doubles at the system's edges: the clock, the terminal, the
  network and other agents' CLIs.
- A function with complex logic may be driven through every case with test
  doubles as its collaborators, when tests through real pieces already cover
  that function. A token-usage forecast, for example, would be tested over
  many numeric inputs, not over transcripts.
- Do not replace any other internal collaborator with a test double: each
  piece's tests would pass while no test runs the two pieces together.

### Mutation testing

A good test fails when the logic it covers changes in any way. Check a test
by breaking that logic, such as inverting a condition, removing a step or
moving a boundary by one, and running the test again. A test that still
passes does not cover that logic.

### Test-only fields

- Observe behaviour through effects the code already has: what the cache
  stores, sentinel text in a fixture, what a test double at the edge
  recorded.
- Add a `#[cfg(test)]` field, counter or accessor to a production type only
  when every alternative costs far more. The pull request states the reason,
  reviewers check it, and a comment beside the field explains why it exists.

### Coverage across agents

- When agents share a behaviour, test it once, over every provider: in
  `src/history/provider/contract_tests.rs`, and through each provider's
  every-record fixture (`ProviderFixture::every_record_session`).
- Add a new record kind to its agent's every-record fixture, so every
  coverage test that reads `every_record_sessions` covers it.
- Keep provider-specific tests to what is specific to that agent's storage
  or records.

### Test size

Size a test matrix by what a missed case costs the user. Sweep every value
only where a missed case would lose data, crash rearview or show stale
content as current. Truncation, layout and similar logic get a sample at
fixed steps, plus one test for each regression found.

### Names and messages

- Test names do not start with an article: `read_fits_each_sampled_budget`,
  not `a_read_fits_each_sampled_budget`. Write `single` or `multiple` when
  the claim depends on quantity.
- A test name that states what does not happen uses `doesnt`, or `dont`
  after a plural subject: `unknown_id_doesnt_link`,
  `empty_hidden_parts_dont_print_atom`.
- Assertion messages state the expectation the assertion checks:
  ``"the shards restore once `sessions.bin` is gone"``. Add the actual value
  when the assertion does not print it:
  `"the override replaces every default: {overridden:?}"`.
- Messages that state the failure, such as
  `"output exceeds budget {budget}: {output}"`, predate this rule. Reword
  one when its test is next edited.

## Text

- Use the words in [GLOSSARY.md](GLOSSARY.md), one for each concept.
- New prose, comments and test names use British spelling. Identifiers from
  libraries keep theirs.
- Write "without", not "with no": "a tool result without text".
- Name a key by its action, then the key in backticks: "session ID copy
  (`I`)", not a bare `I`.
- Name a mode by what it is, such as summary mode (`tools·sum` in the
  status bar), not by the keys that reach it.

## Commits

- A small change is one commit. A larger change is a series of logical
  commits; each message explains its part of the design.
- Put a refactoring, such as a rename, in a commit of its own, and build the
  change on it in the next. Reviewers can then check that the refactoring
  does not change behaviour, and read the logic change alone.
- Squash review changes and corrections into the commit that added the code
  they change: `git commit --fixup=<commit>`, then
  `git rebase --autosquash <base>`. The history then shows each commit as
  written, without review commits, and blame points at the commit that
  explains the code.
- Commit messages:
  - The subject is imperative and names the behaviour that changed, not the
    files or symbols.
  - The body opens with the changed behaviour or result, then the defect,
    constraint or tradeoff that required it.
  - Leave out what the diff shows. The length tracks the change: a small
    fix may need only the subject.
  - Name the tests the commit adds and the behaviour they protect, not that
    existing tests still pass.

## Pull requests

- A pull request changes what its task needs. Leave what you notice along
  the way, such as an existing US spelling, for a change of its own:
  unrelated edits enlarge the diff a reviewer reads.
- The title names the user-visible effect, not the mechanism. A change
  without a user-visible effect starts its title with `(internal) `.
- The body uses these headings, in order:
  1. **The bug**, or **The change** for a feature: the user-visible problem
     or effect.
  2. **After this change**: what the user sees now.
  3. The cause, then the fix, each under its own heading.
  4. **Validation**: two to four lines naming the groups of behaviour the
     tests cover, not individual tests.
- `mise run check` passes.

## Changelog

- Add an entry under `## Unreleased` at the top of `CHANGELOG.md`, creating
  the heading when the last release retitled it. A release retitles that
  heading with its version and date, and refuses to run unless exactly one
  exists; see [RELEASE.md](RELEASE.md).
- Group entries under `### Security`, `### Enhancements`, `### Fixes` and
  `### Changes`. `### Changes` holds a behaviour change that is neither a
  fix nor an enhancement, such as a flag's new argument or a one-time cache
  rebuild.
- Group fixes under `#### All agents`, ``#### `rearview agent` commands``,
  then one `#### <agent>` heading for each agent.
- Group work without a user-visible effect under `### Internal: <area>`.
- A fix starts with the verb and the symptom: "Fix …". A speedup is an
  enhancement, stated as a ratio.
- An entry under `## Unreleased` describes the net behaviour. A later change
  to an unreleased feature or fix updates that entry instead of adding its
  own.
- Do not add an entry for a CI or lint gate.

## Workflows

- **Writing a provider:** [PROVIDERS.md](PROVIDERS.md) covers the registry
  contract every provider follows and each agent's storage.
- **Upkeep with an agent's releases:**
  [workflows/provider-releases.md](workflows/provider-releases.md) reviews
  the releases an agent shipped since its last review, and records them in
  PROVIDERS.md.
- **Releasing rearview:** [RELEASE.md](RELEASE.md) covers the changelog,
  the version bump, publishing and the Homebrew tap.
