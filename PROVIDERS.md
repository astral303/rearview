# Provider support

A provider is one coding agent whose history this tool reads. `Source` identifies
the agent. `SessionProvider` holds everything that differs between agents.

To add an agent, register a provider. Do not add match arms.

```rust
Source::Pi.provider().storage()    // reach behaviour through the registry
provider::providers()              // iterate every registered provider
```

Code lives in `src/history/provider/` (policy) and `src/history/format/` (wire
formats).

The rules the registry enforces for every provider come first, then the agent
releases reviewed so far. Each provider then has a section of its own; Pi and OMP
share one because they share a wire format.

## Registry contract

Every provider has a `storage()` and a `format()`. The methods with a default
body carry the rule every agent shares: the agent CLI's reference recipe
(`ref_project`, `conversation_ref`, `session_id_in_locator`) and the workspace
match (`is_in_workspace`). A provider overrides one only when its agent
differs.

The workspace match places a session by the directory it recorded. A session
is in the workspace when that directory is a checkout of the current
repository or inside one. Outside a repository, the directory must be the
current directory. `Workspace` reads the checkouts once, from the `.git` files,
without running git. A `.git` in the home directory makes no repository.

Code outside each agent's own modules reaches an agent through the registry.
An agent's own modules in `provider/` and `format/` are named after its
`Source` variant in lowercase or snake_case, alone or before an underscore:
`opencode.rs`, a `claude/` directory, `pi_log.rs`.
`tests/shared_code_names_no_agent.rs` fails when other code names a `Source`
variant or a path into an agent's module; comments, strings, test code and the
registry statics may. It does not catch agent-specific behaviour that names no
agent, such as matching sessions by their parent folder's name. Do not start a
shared module's name with an agent's variant in lowercase or snake_case
(`claude_md.rs`): the test counts it as that agent's own.

Reference namespaces are a compatibility contract. Users record references. If you
change a namespace, every recorded reference breaks.
`emitted_refs_and_project_ids_are_pinned` guards this.

Three registry queries answer the questions a caller can have:

| Question                          | Function                                         |
|-----------------------------------|--------------------------------------------------|
| Which format reads this file?     | `format::parse_transcript(path)`                 |
| Does this source own this file?   | `format::parse_owned_transcript(source, path)`   |
| May this source delete this file? | `format::require_owned_transcript(source, path)` |

"Parses" is weaker than "owns". All three fail rather than answer when the file
cannot be read, so a guard never mistakes an unreadable transcript for one that
belongs to somebody else.

Caches live under `~/.cache/rearview/`, or under the directory
`REARVIEW_CACHE_DIR` names. `SessionCacheStore` holds the
directory and the identity together, and takes both from `storage.cache()`, so a
provider cannot stamp a file with one identity and file it under another. The
root hash keeps two roots apart. A root that moves misses the cache. It does not
read stale entries.

A root's session cache is sharded, N=16 today, each session assigned to a
shard by a hash of its cache key. A load rewrites only the shards holding a
session that was reread, recorded empty, or deleted. Opening a
session by ID rewrites its shard. A `sessions.bin` from an earlier release is
migrated on the next load.

An agent's own archive flag is not a reason to skip a session. OpenCode's
`session.time_archived` and Kimi's `archived` in `state.json` mark a session
its agent hides from its own picker, which is the case this browser exists
for, and archiving is reversible where deleting is not. Discovery collects
them; the list and search both show them.

Codex is the exception. Archiving there moves the rollout out of `sessions/`
into `archived_sessions/`, which no root covers, so the session never reaches
discovery.

`[resume].default_args` documents arguments for the `claude` CLI.
`SessionLaunch::configured_args` carries them to every launcher, and only Claude's
applies them.

### Capabilities

Each provider declares each capability below, with no default, as
`Capability::Capable` or `Capability::NotCapable`. Each capability is a
`SessionProvider` method in `provider/mod.rs`. Its doc comment opens with a
"Capable when" sentence.

The table is generated from those sentences and the providers' declarations.
After editing either, run `mise run update-capability-table`; a test fails
while the table is out of date.

<!-- generated capability table: begin -->

| Capability                 | Capable when                                                                                        | Not Capable                   |
|----------------------------|-----------------------------------------------------------------------------------------------------|-------------------------------|
| `resume`                   | the agent's command line resumes a session                                                          | —                             |
| `fork`                     | the agent's command line starts a session branching from another                                    | Kimi                          |
| `rename`                   | a session can be given a user-chosen title                                                          | —                             |
| `delete`                   | a session and what the agent stores for it can be deleted                                           | —                             |
| `subagents`                | discovery names sub-agent transcripts on their session's stub, and a delete removes and counts them | Pi                            |
| `subagent_by_id`           | a sub-agent transcript's ID resolves through `resolve_session_id` to a stub of its own              | Claude, Kimi, Pi, OMP         |
| `session_id_lookup`        | a typed session ID resolves through `is_session_id_shape` and `resolve_session_id`                  | Pi, OMP                       |
| `workspace_sessions_dir`   | the agent stores the sessions recorded in each working directory in a directory of their own        | Codex, OpenCode               |
| `current_session_variable` | each shell the agent runs gets an environment variable holding its session's ID                     | OpenCode, Kimi, Grok, Pi, OMP |

<!-- generated capability table: end -->

`subagent_by_id` is Capable only where `subagents` and `session_id_lookup` are.

The table shows OpenCode's `fork` as Capable because unit tests read
OpenCode's version as unknown. The OpenCode provider reads
`opencode --version` once per run, on first use, and declares `fork` Not
Capable with OpenCode 2 installed and Capable with any other version or an
unknown one. `fork_if_known` returns the `fork` declaration where it is
known without reading, else `None`. The actions menu reads it; where it
returns `None`, the menu reads `fork` on a background thread and shows
`<loading…>` in place of Fork.

A Not Capable declaration carries a `DenialDetail` only when the user needs
guidance once the app refuses the operation. The app composes the refusal
with the agent's display name:

| Detail                 | Declared by                                                                  | Refusal                                                               |
|------------------------|------------------------------------------------------------------------------|-----------------------------------------------------------------------|
| `ResumeThenForkInside` | `fork` for Kimi, and for OpenCode with OpenCode 2 installed                  | `<agent> forks only from inside a session; resume it, then use /fork` |
| none                   | `resume`, `fork`, `rename` or `delete` declared Not Capable without a detail | `<agent> cannot <operation> a session`                                |

Each surface reads a declaration and runs through what a Capable one carries:

| Surface                                 | Declaration it reads                                                                                                                               | Where Not Capable                                               |
|-----------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------|
| The actions menu                        | `resume`, `fork`, `delete` and `rename` of the row's agent, through their `SessionResumer`, `SessionForker`, `SessionDeleter` and `SessionRenamer` | greys the action out                                            |
| The rename key                          | `rename`, through its `SessionRenamer`                                                                                                             | renames nothing                                                 |
| `--resume`, `--fork-session`            | `resume` and `fork`, through their `SessionResumer` and `SessionForker`                                                                            | reports the refusal and builds no command                       |
| `--delete`, `delete-empty`              | `delete`, through its `SessionDeleter`                                                                                                             | reports the refusal                                             |
| A bare file (`--render`, a direct path) | `subagents`, through its `SubagentFinder`, when the agent stores the file                                                                          | shows no sub-agent transcript                                   |
| The search box, `agent read <id>`       | `session_id_lookup`: the registry's `is_session_id_shape` and `resolve_session_id` call its `SessionIdResolver`                                    | the registry's session-ID lookups skip the provider             |
| `agent search`                          | `current_session_variable`, carrying the variable's name                                                                                           | leaves out no session of that agent                             |
| `--show-dir`, `--show-all-dirs`         | `workspace_sessions_dir`, carrying the lookup from a working directory to its sessions directory                                                   | `--show-dir` refuses the agent; `--show-all-dirs` leaves it out |

`--show-dir` and `--show-all-dirs` run the lookup for the current directory,
each parent up to the repository's top level, and from a linked worktree the
main checkout's top level, and print the first sessions directory holding a
session the provider's discovery lists (`found_sessions_dir`).

A provider's capability declarations decide which contracts in
`provider/contract_tests.rs` run for it; `Contract::runs_for` maps each
contract to the declarations that turn it on, Not Capable ones included. A
contract no provider's declarations turn on fails.

## Upstream releases

This table records which agent releases have been reviewed, so a storage change
in a new release is found before a user reports a missing session.
[`workflows/provider-releases.md`](workflows/provider-releases.md) gives the
steps for a review.

- **Reviewed through**: the newest release with its notes and storage changes
  read.
- **Accounted for through**: the newest release with every finding resolved.
- **Last review**: the date of the last review or resolution, written
  `YYYY-MM-DD`.

Versions name stable releases and are bare: digits and dots (`0.153.4`, not
`v0.153.4` or `0.154.0-alpha.1`). They compare in version-sort order. `—` in
both version columns and "Last review" marks an agent not reviewed yet. `—` in
"Accounted for through" alone means no release is accounted for: the earliest
open finding dates from the agent's first stable release.

| Agent    | Release source                                                                                          | Reviewed through | Accounted for through | Last review |
|----------|---------------------------------------------------------------------------------------------------------|------------------|-----------------------|-------------|
| Claude   | [changelog](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md)                           | 2.1.296          | 2.1.296               | 2026-10-13  |
| Codex    | [releases](https://github.com/openai/codex/releases)                                                    | 0.162.1          | 0.162.1               | 2026-10-13  |
| OpenCode | [tags](https://github.com/anomalyco/opencode/tags)                                                      | 2.0.26           | 2.0.26                | 2026-10-12  |
| Kimi     | [releases](https://github.com/MoonshotAI/kimi-code/releases)                                            | 2.1.1            | —                     | 2026-10-13  |
| Grok     | [changelog](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-shell/CHANGELOG.md) | 1.0.45           | 1.0.45                | 2026-10-11  |
| Pi       | [releases](https://github.com/earendil-works/pi/releases)                                               | 1.1.0            | 1.1.0                 | 2026-10-12  |
| OMP      | [releases](https://github.com/can1357/oh-my-pi/releases)                                                | 18.10.3          | —                     | 2026-10-12  |

Each agent's section ends with `### Upstream findings: <agent>`: one bullet per
finding, written `- **<status>** (<version>): <what changed>`. The version is the
release that introduced the change. Sub-bullets under a finding hold detail and
are not checked.

| Status             | Meaning                                                                            | Holds "accounted for through" back |
|--------------------|------------------------------------------------------------------------------------|------------------------------------|
| `needs evaluation` | not judged yet                                                                     | yes                                |
| `pending support`  | rearview should read it and does not yet                                           | yes                                |
| `deferred`         | not supported for now, such as an experimental change; the bullet gives the reason | no                                 |

Open findings (`needs evaluation`, `pending support`) fall after "accounted for
through" and no later than "reviewed through"; a deferred finding falls no later
than "reviewed through". A finding is settled and resolved as the runbook's
"Settle judged findings" and "Resolve a finding" steps describe. An agent not
reviewed yet can only have `needs evaluation` findings.
`provider/upstream_releases.rs` fails when this table, the registry, the
findings and the files in `workflows/provider-releases/` disagree.

## Claude

| Method                                 | Value                                         |
|----------------------------------------|-----------------------------------------------|
| `labels().name` / `.list` / `.display` | `claude` / `CC` / `Claude`                    |
| `storage()`                            | `ClaudeStorage`                               |
| `format()`                             | `CLAUDE_TRANSCRIPT`                           |
| `resume()` / `fork()`                  | `ClaudeLauncher`                              |
| `ref_namespaces().conversation`        | `agent-v1`                                    |
| `ref_namespaces().project`             | `agent-project-v1`                            |
| `current_session_variable()`           | `CLAUDE_CODE_SESSION_ID`                      |
| `workspace_sessions_dir()`             | the project folder `--show-dir claude` prints |

Claude writes `LogEntry` records with no session header. Its format recognizes
a file that holds any Claude record: the session ID is the file name, the start
time is the first record's, and the cwd is the first user record's. Its row and
viewer entries are read from the file line by line, not from the projection
`parse_transcript` returns, so a row keeps each malformed line's text for
`--debug`. The view splices each sub-agent transcript under the label its
sidecar names, without the records that repeat the `Agent` call that launched
it. A fork's transcript opens with a copy of its launching `Agent` call, usage
included. The row counts the text and tokens of the records the view splices,
so its launching call is counted once. `assign_canonical_tools` in
`format/claude.rs` sets each tool call's canonical `Tool` after deserializing.

Claude overrides two provider defaults:

- **References** digest the project folder's name and the file name, the
  inputs of every Claude `ch_` reference users have recorded. The path names
  every key, so the agent CLI keys a Claude session without reading it, and a
  transcript that holds no conversation or cannot be read still gets a key,
  so `agent search` reports it.
- **Workspace match**: a session stored in the project folder of a checkout,
  or of a worktree under its `.worktrees/` or `.claude/worktrees/`, is in the
  workspace whatever directory it recorded. A session that recorded none has
  only its folder, and rearview's cross-project resume or fork copy keeps the
  original's directory. A folder name matches whole, or up to one of those
  worktree names: `<repo>-lane` beside the repository encodes as the
  repository's folder name followed by `-lane`, and does not match.

| Storage               | Value                                                                                                            |
|-----------------------|------------------------------------------------------------------------------------------------------------------|
| Default root          | `~/.claude/projects`                                                                                             |
| Layout                | one directory per project, named for the encoded cwd, holding `<session-id>.jsonl`                               |
| Root override         | `$CLAUDE_CONFIG_DIR/projects`; an empty value means unset                                                        |
| Project               | the transcript's first cwd, else the path the folder name decodes to                                             |
| Sub-agent transcripts | `<session-id>/subagents/agent-<id>.jsonl`, `agent-<id>.meta.json` beside each; nested ones in the same directory |
| Sub-agent label       | the `agentType` its sidecar names                                                                                |
| Excluded files        | `agent-*.jsonl` beside the sessions, an older flat layout                                                        |
| Cache directory       | `claude/`                                                                                                        |
| Cache magic / schema  | `CLHIST02` / 15                                                                                                  |

Releases up to v0.3.1 cached Claude per project directory, in `projects/` under
the cache directory. Each load deletes the cache files left there, then the
folder once it is empty; any other file in it stays.

Claude keeps each sub-agent transcript at
`<project>/<session-id>/subagents/agent-<id>.jsonl`, the same JSONL as a
session, with `agent-<id>.meta.json` beside it naming the `agentType` (the
splice label), the `toolUseId` of the `Agent` call that ran it, and its
`spawnDepth` (not read). A nested sub-agent's transcript sits
in the same directory. Claude's loader names them per session as a storage's
discovery would, with one `read_dir` on a directory most sessions do not
have, and reads them through the same merge and splice as the other
providers. A sidecar that is absent or names no `agentType` leaves the thread
labelled with its agent id. A `subagents/` directory that cannot be read is
reported at warn level and treated as empty by the loader, session-ID lookup
and delete alike. `agent-*.jsonl` beside the sessions, an older flat layout,
names its session only in its own records and is skipped.

Claude Code records a finished background agent or background shell command as
a user message wrapping `<task-notification>`.
`history::task_notification::parse_task_report` reads its `summary`, `result`
and `usage` fields and ignores the rest (`task-id`, `tool-use-id`,
`output-file`, `status`, `note`, `worktree`). The viewer, `--render`, export,
the agent CLI, the preview and both search indexes read the parsed text; a
notification with no closed `summary` renders under `You` as written. A
background agent that is a sub-agent splices in as a sub-agent thread; the
`Task` row follows the thread, where the notification sits.

From 2.1.63 Claude Code records a local slash command and its output as
`system` records with subtype `local_command`, and some commands' output as a
user message. `format::claude::parse_record` reads a `local_command` record as
a user message holding its text. The row is built from the entries the viewer
shows. `agent read` shows a command's output and error as the viewer does,
each as a message; empty output, such as `/clear`'s, is left out.

From 2.1.97 Claude Code writes a `queued_command` attachment for each message
that arrives while Claude is working. The record's `timestamp` is when the
message was queued, often earlier than the record before it. `origin.kind`, or
`commandMode` in a record without one, names the sender. `parse_record` reads
a message from the user, a task notification or a sub-agent's hand-back as a
user message holding its `prompt`; a task notification and a hand-back then
show as `Task` rows. A coordinator's message, the parent session writing to a
sub-agent, is skipped: it repeats the parent's `SendMessage` call. A message
from an unknown sender is not shown.

From 2.1.97 Claude Code writes an attachment for each file or directory
attached with `@`, and for each file it attaches again after a compaction.
`parse_record` reads each attachment type as:

| Attachment                  | Shows as                                                      |
|-----------------------------|---------------------------------------------------------------|
| `file`, `already_read_file` | a `Read` call under `You`, with the file's text as its result |
| `directory`                 | a `List dir` call under `You`, with the listing as its result |
| `compact_file_reference`    | a `Reference` call under `You`, with no result                |

- Each call uses the path Claude Code shows, `displayPath`.
- An attachment holding part of a file gives the `Read` call that line range.
- An image, PDF or notebook attachment, and a file unchanged since Claude read
  it, show the `Read` call without a result.
- Claude Code writes a `compact_file_reference` after a compaction for a file
  too large to attach again; it holds no content.
- In summary mode a `Reference` call counts as `referenced N files`.

From 2.1.97 Claude Code writes a hook's message as a `hook_system_message`
attachment and a hook's error as a `hook_non_blocking_error` attachment. Each
carries the `toolUseID` of its hook's call.

- The projection moves each attachment's text into the result of that call:
  before the result's content when the attachment comes first in the file,
  after it otherwise.
- A hook's text is `<hookName> hook: <content>`, or `<hookName> hook error:
  <stderr>`.
- A call with no result takes its first hook attachment as its result.
- A hook attachment with no call becomes a `Hook` row reading
  `<hookName>: <content>`, or `<hookName> error: <stderr>`. Stop,
  UserPromptSubmit and SessionStart hooks carry a random `toolUseID`.
- A Stop or SubagentStop hook's error attachment is skipped. The
  `stop_hook_summary` record after it lists the same error, and each error
  there becomes a `Hook` row, `Stop error: <stderr>`.

From 2.1.97 Claude Code writes IDE and language-server errors and warnings as
a `diagnostics` attachment. `parse_record` reads it as an `Issues` report:

- The summary is Claude Code's count line, `Found N new diagnostic issues in
  M files`.
- The body lists each file's path, then `[Line <l>:<c>] <message> [<code>]
  (<source>)` per issue, truncated as a task report's body is.
- `parse_record` writes the report in an `<ide-diagnostics>` wrapper. The
  viewer, export, search and the agent CLI parse it as they parse a
  `<task-notification>`.

From 2.1.108 Claude Code writes a session recap as a `system` record, and
from 2.1.229 a safeguard refusal warning. `parse_record` reads each subtype as:

| `system` subtype                          | Shows as                                                                             |
|-------------------------------------------|--------------------------------------------------------------------------------------|
| `away_summary`                            | a `Recap` row, without the trailing `(disable recaps in /config)`                    |
| `model_refusal_fallback`, `informational` | a `Notice` row                                                                       |
| `model_refusal_no_fallback`               | skipped: its `content` is empty, and the API error reply after it shows the refusal |

- The viewer and exports show a `Recap` row behind the thinking toggle.
- Every `informational` record shows as a `Notice` row, backgrounding notices
  included: no field separates a safeguard notice from the others.
- The changelog does not date safeguard refusal warnings; the earliest example
  is from 2.1.229.
- Claude Code follows a safeguard refusal warning with a note to the model: a
  `user` record marked `isMeta`, with the stopped reply's `refusedMessageId`.
  `parse_record` skips the note, and the dropped-records report counts it as
  skipped under `type=user`. The earliest example is from 2.1.295.

| Operation               | Behavior                                                                       |
|-------------------------|--------------------------------------------------------------------------------|
| Resume                  | `claude --resume <id>`, in the directory that maps to the transcript's project |
| Fork                    | `claude --resume <id> --fork-session`, in the current directory                |
| Cross-project fork      | copies transcript and session directory                                        |
| `[resume].default_args` | applied                                                                        |
| Rename                  | appends `custom-title` and `agent-name` records                                |
| Delete                  | removes every copy, by session ID, that Claude's format owns, each with its session directory |

Claude resumes by ID and finds the transcript through the directory it runs in. When
the session sits under a project Claude does not search, the launcher copies the files
to one it does. Building a Claude command therefore writes to disk. This is why a
launcher returns a command instead of running one.

### Upstream findings: Claude

None recorded.

## Pi and OMP

Pi and OMP share one wire format. Two `PiLogFormat` statics read it. They differ only
in the source they attribute a transcript to.

| Method                                 | Pi                                         | OMP                                         |
|----------------------------------------|--------------------------------------------|---------------------------------------------|
| `labels().name` / `.list` / `.display` | `pi` / `Pi` / `Pi`                         | `omp` / `OMP` / `OMP`                       |
| `storage()`                            | `PiStorage`                                | `OmpStorage`                                |
| `format()`                             | `PI_LOG`                                   | `OMP_LOG`                                   |
| `resume()` / `fork()`                  | `PathResumeLauncher`                       | `PathResumeLauncher`                        |
| `ref_namespaces().conversation`        | `agent-pi-v1`                              | `agent-omp-v1`                              |
| `ref_namespaces().project`             | `agent-pi-project-v1`                      | `agent-omp-project-v1`                      |
| `workspace_sessions_dir()`             | the child directory `--show-dir pi` prints | the child directory `--show-dir omp` prints |

| Storage                     | Pi                                                             | OMP                                                                                                                      |
|-----------------------------|----------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------|
| Default root                | `~/.pi/agent/sessions/`                                        | `~/.omp/agent/sessions/`                                                                                                 |
| Layout                      | child directories                                              | child directories                                                                                                        |
| Root overrides              | `PI_CODING_AGENT_DIR`                                          | `PI_CODING_AGENT_DIR`, `PI_CONFIG_DIR`, `OMP_PROFILE`, `PI_PROFILE`, `XDG_DATA_HOME`                                     |
| Session directory overrides | `PI_CODING_AGENT_SESSION_DIR`, `sessionDir` in `settings.json` | `PI_CODING_AGENT_SESSION_DIR`                                                                                            |
| Sub-agent transcripts       | none: Pi does not support sub-agents                           | the `.jsonl` files directly inside `<name>/`, the artifacts directory beside each `<name>.jsonl`, at every nesting level |
| Sub-agent label             | —                                                              | its file stem, the output id (`Parent.Child` for a nested one)                                                           |
| Excluded files              | none                                                           | none                                                                                                                     |
| Cache files                 | `pi/root-<hash>/shard-NN.bin`                                  | `omp/root-<hash>/shard-NN.bin`                                                                                           |
| Cache magic / schema        | `PIHIST01` / 13                                                | `OMHIST01` / 15                                                                                                          |

- A root override moves the root; the agent still stores each working
  directory's sessions in a child directory of its own under it.
- A session directory override holds every working directory's sessions
  directly inside it. OMP has no `sessionDir` setting.

`workspace_sessions_dir` names the child directory holding a working
directory's sessions, with `/`, `\` and `:` in the path replaced by `-`:

- Pi: `--<path>--`, the path without its leading separator.
- OMP: `-tmp-<path below it>` below the temporary directory,
  `-<path below it>` below the home directory, `--<path>--` elsewhere. OMP
  first resolves symbolic links in the working, home and temporary
  directories.
- OMP started in the home directory moves to the first of `~/tmp`, `/tmp`
  and `/var/tmp` that exists (`~/tmp` alone on Windows), else to the
  temporary directory, and names that directory's sessions directory.
- Under a session directory override, `workspace_sessions_dir` names the
  override itself.

| Transcript format             | Pi                                            | OMP                                           |
|-------------------------------|-----------------------------------------------|-----------------------------------------------|
| Records                       | Pi log JSONL                                  | Pi log JSONL                                  |
| Session header                | `{"type":"session", ...}`                     | same, after `title` slot                      |
| Header fields                 | `id`, `timestamp`, `cwd`, `version` 1–3       | same                                          |
| File states its agent         | no                                            | yes, through `title` slot                     |
| Agent when the file is silent | Pi                                            | OMP in OMP's own tree, Pi in a redirected one |
| Branching                     | `parentId` chain; only active branch projects | same                                          |
| `/clear` boundary             | —                                             | `reset_boundary` becomes a `Context cleared` row |

A transcript with no `title` slot reads equally well as Pi or as OMP. The root that
holds the file decides. This is not cosmetic: it labels every assistant turn in the
viewer.

`SessionRoot::origin()` answers which case a root is. The resolver in
`omp_loader.rs` records it, because only the resolver knows: the config tree and
the XDG data directory are OMP's own, and the `PI_CODING_AGENT_DIR` and
`PI_CODING_AGENT_SESSION_DIR` overrides point at a directory the user chose,
where Pi can write too. The answer is not recoverable from the path afterwards —
a home directory can itself be named `omp`. Roots are redirected unless
`SessionRoot::in_agent_tree()` says otherwise, so a provider that says nothing
cannot claim a transcript that names no agent.

OMP writes a session's artifacts to a directory of the same name beside its
transcript, `.jsonl` stripped. A sub-agent's transcript is `<outputId>.jsonl`
directly inside it, a nested sub-agent's dot-qualified (`Parent.Child.jsonl`)
in the same directory, each beside a `<outputId>.md` report that is not read.
An artifacts directory that cannot be read is reported at warn level and
treated as empty, so the session still lists and deletes. A redirected root
applies the same rule at its own depth; its session files are attributed by
the registry, and its artifacts directories are read as OMP's. Layout per
OMP's `docs/blob-artifact-architecture.md` at `d17c270`.

| Operation               | Pi                            | OMP                                           |
|-------------------------|-------------------------------|-----------------------------------------------|
| Resume                  | `pi --session <path>`         | `omp --resume <path>`                         |
| Resume runs in          | session project directory     | session project directory                     |
| Fork                    | `pi --fork <path>`            | `omp --fork <path>`                           |
| Fork runs in            | current directory             | current directory                             |
| `[resume].default_args` | ignored                       | ignored                                       |
| Rename                  | appends `session_info` record | rewrites `title` slot, appends `title_change` |
| Delete                  | removes file                  | removes file and artifacts directory, reporting the sub-agent transcripts in it |

### Upstream findings: Pi

None recorded.

### Upstream findings: OMP

- **pending support** (1.5.0): calls to OMP's `grep`, `find`, `web_search` and
  `task` tools show their input as JSON and count as `called N tools` in
  summary mode.
  - `grep` was named `search` from 14.5.5 through 16.1.23.
  - The 1.5.0 `find` searched for files by glob pattern. 16.2.0 renamed it
    `glob`, and rearview reads `glob` calls.
- **pending support** (1.5.0): edits outside hashline mode show their input as
  JSON. The `replace`, `patch`, `apply_patch` and `sloppy` modes each send a
  different input.
  - `replace` is the default for Kimi, MiniMax, DeepSeek and a few other
    models.
- **pending support** (1.5.0): files OMP reads for an `@path` mention are
  missing from the viewer and search. OMP records them as a `fileMention`
  message and shows `Read <path> (N lines)` for each.
- **pending support** (6.9.69): Python the user ran with `$` and its output
  are missing from the viewer and search. OMP records the run as a
  `pythonExecution` message and shows the code and its output.
- **pending support** (16.0.11): an advisor's transcript shows as a sub-agent.
  OMP writes it as `__advisor.jsonl` in the artifacts directory of the session
  or sub-agent it advises.
  - From 16.2.3 a named advisor writes `__advisor.<slug>.jsonl`.
  - The advisor is opt-in. OMP keeps the transcript for usage accounting and
    shows the advisor's notes in the session.
- **pending support** (18.1.0): tokens from model calls outside the
  conversation are missing from the session's token count. OMP records each
  call as a `model_usage` entry and counts it in its session totals.
- **pending support** (18.2.7): calls to OMP's new `find` tool show their input
  as JSON and count as `called N tools` in summary mode.
  - The new `find` returns files and line ranges for a natural-language
    description. By default OMP turns it on when its `judge` role resolves
    first to a native judgment backend.
- **pending support** (18.3.0): calls to OMP's `wait` tool show their input as
  JSON and count as `called N tools` in summary mode.

## Codex

| Method                                 | Value                     |
|----------------------------------------|---------------------------|
| `labels().name` / `.list` / `.display` | `codex` / `CDX` / `Codex` |
| `storage()`                            | `CodexStorage`            |
| `format()`                             | `CODEX_ROLLOUT`           |
| `resume()` / `fork()`                  | `CodexLauncher`           |
| `ref_namespaces().conversation`        | `agent-codex-v1`          |
| `ref_namespaces().project`             | `agent-codex-project-v1`  |
| `current_session_variable()`           | `CODEX_THREAD_ID`         |

| Storage                    | Value                                                                                                                                         |
|----------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------|
| Default root               | `~/.codex/sessions/`                                                                                                                          |
| Layout                     | dated tree: `<YYYY>/<MM>/<DD>/rollout-<stamp>-<thread>.jsonl`                                                                                 |
| Root override              | `CODEX_HOME` (moves the whole home)                                                                                                           |
| Excluded files             | files not named as rollouts; a thread's earlier rollouts, read through the newest                                                             |
| External titles            | `session_index.jsonl`, beside the sessions tree                                                                                               |
| Session list               | `state_5.sqlite`, beside the sessions tree (rollout headers when the file is absent)                                                          |
| Session, from the database | a `threads` row no edge names as a sub-agent, other than a skipped one; its sub-agent threads are the rows beneath it by `thread_spawn_edges` |
| Session, from the headers  | a rollout whose header names no parent, or names one with no rollout; its sub-agent threads are every rollout whose header chain reaches it   |
| Schema pin                 | `_sqlx_migrations` version 59                                                                                                                 |
| Cache files                | `codex/root-<hash>/shard-NN.bin`                                                                                                              |
| Cache magic / schema       | `CXHIST01` / 17                                                                                                                               |

The session state database is Codex's own list of its threads:

- A `threads` row names each thread's rollout and its `source`; a
  `thread_spawn_edges` row names each sub-agent thread and its parent.
- The absolute `rollout_path` is re-rooted onto the sessions tree, so a
  copied Codex home still lists its sessions.
- A row whose rollout is absent is dropped; one whose rollout Codex
  compressed counts as `compressed sessions unsupported`.
- A rollout with no `threads` row is not listed.
- A database at a version newer than the one this release was developed
  against is still read; `--debug` warns once.

When the database file is absent, discovery reads the rollout headers instead.
A database that is present but locked, cannot be opened, fails a query, or
names no session file stops the Codex load for that launch; the `Ctrl+L` list
and `agent search` report the reason. The failure is
`AppError::SessionListUnreadable { reason, detail }`: the loader shows `reason`
as the term `Codex │ <reason>: sessions not loaded`, and `--debug` prints
`detail`, the file and the SQLite failure. The first three reasons are chosen
in `provider/sqlite.rs`, shared with OpenCode.

| Reason                                    | Condition                                                                                      |
|-------------------------------------------|------------------------------------------------------------------------------------------------|
| `session database locked`                 | another connection held the database past the 5-second busy wait; Codex is most likely writing |
| `session database cannot be opened`       | the file cannot be opened, a directory in its place included                                   |
| `session database cannot be read`         | a query failed: not a SQLite file, or a table missing                                          |
| `session database names no session file`  | rows exist and none names a rollout under the tree, plain or compressed: another tree's database |

Editing an earlier prompt continues the thread in a new rollout. The newest
rollout is the file Codex resumes, and discovery lists only that file, from the
headers or the database.

- A rollout's `history_base` names the rollout it continues, by rollout id, and
  the byte just past the last record it inherits: the cut.
- A paginated fork's `history_base` names a rollout of the thread it forked
  from.
- The reader finds each earlier rollout by its id in the sessions tree or in
  `archived_sessions` beside it, walking both once per read, and reads them
  oldest first, each up to where the next continues it.
- A rollout missing from both trees, compressed or permanently unreadable ends
  the chain: the rollouts after it are read, and the ones before it are not.
- When the chain ends at a missing, compressed or permanently unreadable
  rollout, the row's `is_missing_earlier_turns` is set and cached. `Ctrl+L` in
  the list reports how many loaded rows have it:
  `Codex │ N sessions missing earlier turns`.
  - A transient error also ends the chain and sets
    `is_missing_earlier_turns`, but leaves the row uncached: a refresh reads
    the session again.
- Past the cut, a thread's own earlier rollout holds the discarded turns.
  Each `spawn_agent` result there names the sub-agent by
  `agent_id`, or by its agent path in `task_name`. The session's view, search
  text and counts leave out those sub-agents and every thread beneath them;
  each opens by its session ID and is deleted with the session.
  - A sub-agent's header carries its path as `agent_path`. A path matches
    when the sub-agent's header `timestamp` is earlier than the start of the
    thread's next rollout: a later sub-agent can reuse the path.
  - The reader does not read a fork's earlier rollout past the cut: those
    records are the forked thread's later turns.
  - A transient error reading those turns or a sub-agent's header leaves the
    row uncached. After a permanent one, the unread turns' sub-agents show.
- An edited prompt in an earlier rollout continues that rollout. The thread's
  rollouts between them leave the chain: all their turns are discarded, and
  their sub-agents are left out too.

Thread titles live in `session_index.jsonl`, so a rename never touches the
rollout the cache validates against; `external_titles` overlays them on warm
loads.

Codex classifies each thread by its `source`, the JSON the `threads.source`
column stores and the newest rollout's header carries; a Guardian ("approve for
me") review, for example, carries `subagent.other`. Classification from the
database:

| `threads` row                                                                                                      | classified as                         |
|--------------------------------------------------------------------------------------------------------------------|---------------------------------------|
| a `child_thread_id` in `thread_spawn_edges`, whatever its `source`                                                 | sub-agent                             |
| `subagent.review`, `subagent.compact`, `subagent.memory_consolidation`, `subagent.other`, `internal`, with no edge | skipped, with every thread beneath it |
| any other, a `thread_spawn` row with no edge included                                                              | session                               |

Classification from the headers:

| `source`                                                                                             | classified as                         |
|------------------------------------------------------------------------------------------------------|---------------------------------------|
| `thread_spawn` (bare string or object) with `parent_thread_id`                                       | sub-agent                             |
| `subagent.review`, `subagent.compact`, `subagent.memory_consolidation`, `subagent.other`, `internal` | skipped, with every thread beneath it |
| absent                                                                                               | by `parent_thread_id` alone           |
| any other                                                                                            | session                               |

A skipped thread is neither listed nor read, is not reported as an ignored
session, and is deleted with the thread it names; the database records no
parent for it, so delete reads its header. Its ID resolves to nothing.
`--debug` prints the count. Discovery from the headers costs one first-line
read per rollout; from the database it opens no rollout.

| Transcript format     | Value                                                                          |
|-----------------------|--------------------------------------------------------------------------------|
| Records               | rollout JSONL: `{{timestamp, ordinal?, type, payload}}`                        |
| Session header        | the first `session_meta` line                                                  |
| Header fields         | `id`, `timestamp`, `cwd`, `parent_thread_id`, `subagent_history_start_ordinal` |
| File states its agent | yes, through the header                                                        |
| Title                 | newest `session_index.jsonl` record naming the thread                          |
| Sub-agent threads     | one rollout per thread, linked by `parent_thread_id`                           |
| Earlier rollouts      | named by `history_base`, read oldest first                                     |
| Compaction            | `compacted` events become invisible metadata                                   |

The reader is a projection:

- `response_item` lines carry the dialogue.
- A `web_search_call` response item becomes a call with no result: Codex saves
  none.
  - A search within a page counts as a web search, in Codex's words:
    `'<pattern>' in <url>`.
- `token_count` events carry usage.
- `turn_context` names the model.
- A `turn_aborted` event becomes a `Notice` row for an interrupted turn or a
  goal-budget stop, in Codex's words; a replaced turn or an ended review shows
  none.
- A `thread_goal_updated` event becomes a `Goal` row when the goal or its
  status changes; a usage change alone shows none.
- Other lines are skipped without being an error, among them context
  snapshots, encrypted reasoning and telemetry.

A sub-agent rollout restates the parent's history below
`subagent_history_start_ordinal`; those lines are skipped so the parent's text and
tokens are not counted twice. A rollout Codex's legacy-to-paginated migration
rewrote sets that ordinal past its last record; nothing is skipped there, and the
whole file is the thread's own history.

| Operation               | Behavior                                                                                |
|-------------------------|-----------------------------------------------------------------------------------------|
| Resume                  | `codex resume <thread-id>`, in the session project directory                            |
| Fork                    | `codex fork <thread-id>`, in the current directory                                      |
| `[resume].default_args` | ignored                                                                                 |
| Rename                  | appends a record to `session_index.jsonl`                                               |
| Delete                  | removes every rollout of the thread and its sub-agent threads, then their index records |

Delete reads the sub-agent threads from the database when its file is present
and from the rollout headers when it is absent. Either way it walks the
sessions tree for the thread's earlier rollouts. With a present database it
cannot use, delete fails with the load's reason before any file is removed.

Delete also removes every deleted thread's archived files from
`archived_sessions`, as Codex's own delete does.

- Delete finds a sub-agent thread with every file archived, nested ones
  included, by reading the archived files' headers.
- A transient error listing `archived_sessions` or reading a header there
  refuses the delete with a retry message; any other error fails it. Either
  way nothing is removed.

- Delete refuses and removes nothing while a fork in the sessions tree or
  `archived_sessions` has a `history_base` pointing at one of the rollouts
  being deleted. The refusal names the forks. Codex refuses the same delete.
- Delete also refuses, with a retry message, when a transient error leaves a
  directory or a rollout header unread: an unread rollout could be a fork. A
  header with a permanent error is passed over: Codex cannot open that
  rollout either.
- A compressed (`.jsonl.zst`) fork is not checked.

Delete leaves the database and `thread_history_1.sqlite` alone; their
version-suffixed schema is Codex's to migrate. The rows left behind cost disk,
not behaviour: Codex drops a thread whose rollout is missing from its own
list.

### Upstream findings: Codex

- **deferred** (0.137.0): compressed rollouts (`.jsonl.zst`) are ignored and
  reported as `compressed sessions unsupported`. Deferred while Codex's
  compression is experimental and off by default.

## Kimi Code

| Method                                 | Value                                            |
|----------------------------------------|--------------------------------------------------|
| `labels().name` / `.list` / `.display` | `kimi` / `KIMI` / `Kimi`                         |
| `storage()`                            | `KimiStorage`                                    |
| `format()`                             | `KIMI_WIRE`                                      |
| `resume()`                             | `KimiLauncher`                                   |
| `ref_namespaces().conversation`        | `agent-kimi-v1`                                  |
| `ref_namespaces().project`             | `agent-kimi-project-v1`                          |
| `workspace_sessions_dir()`             | the workspace directory `--show-dir kimi` prints |

| Storage               | Value                                                                                                             |
|-----------------------|-------------------------------------------------------------------------------------------------------------------|
| Default roots         | `~/.kimi-code/sessions/` and legacy `~/.kimi/sessions/`                                                           |
| Layout                | `<workspace>/<session>/agents/<agent>/wire.jsonl`                                                                 |
| Root override         | `KIMI_CODE_HOME` (replaces both defaults)                                                                         |
| Excluded files        | everything not named `wire.jsonl`                                                                                 |
| Session               | the main wire, `agents/main/wire.jsonl` or the legacy `wire.jsonl`; a directory with no main wire lists each wire |
| Sub-agent transcripts | the other `agents/*/wire.jsonl` in the session directory                                                          |
| Sub-agent label       | the agent part of its `<session>#<agent>` ID                                                                      |
| External titles       | each session's `state.json`                                                                                       |
| Cache files           | `kimi/root-<hash>/shard-NN.bin`                                                                                   |
| Cache magic / schema  | `KIHIST01` / 14                                                                                                   |

Kimi names a workspace directory `wd_<slug>_<hash>`: a slug of the working
directory's last component, and the first 12 hex digits of the SHA-256 hash
of the working directory with `/` separators and no trailing one.
`workspace_sessions_dir` names it under the current home with the id
`workspaces.json` holds for the same working directory:

- the computed id, when the catalogue holds it;
- else the id of the first catalogued root, in file order, that matches;
- else the computed id.

A root matches with or without a trailing separator. A drive-letter or UNC
root matches in any case; a POSIX root matches case-sensitively. Kimi skips
an entry without `name`, `created_at` or `last_opened_at`, and so does the
lookup.

A legacy session keeps its one wire directly in the session directory and names
its working directory `workDir`; both layouts are read. Titles live in
`state.json`, so a rename — Kimi's or this browser's — never touches the wire the
cache validates against; `external_titles` overlays them on warm loads.

| Transcript format       | Value                                                          |
|-------------------------|----------------------------------------------------------------|
| Records                 | wire JSONL: one runtime event per line                         |
| Session header          | first line: `{{"type":"metadata","protocol_version":...}}`     |
| Header fields           | `created_at`; everything else comes from outside the file      |
| File states its agent   | yes, through the metadata line                                 |
| Identity                | the `session_<uuid>` directory; sub-agents `<session>#<agent>` |
| Title / cwd / main agent | the session's `state.json`                                    |
| Compaction              | `context.apply_compaction` becomes invisible metadata          |

The reader is a projection: `context.append_message` carries the user's turns,
loop events carry the assistant's text and tool traffic, turn-scoped
`usage.record` events carry the model and tokens, and everything else — LLM
request payloads, permission prompts — is skipped without being an error.
`turn.prompt` and `turn.steer` restate appended messages and are not read twice;
session-scoped `usage.record` events restate running totals and are not counted.

Each `context.append_message` names its sender in `message.origin.kind`. The
reader shows each kind as Kimi shows it when it reopens a session:

| `origin.kind` | Shows as |
| --- | --- |
| `user`, or none | the prompt under `You`, without a `user` message's bundled skill files |
| `background_task` (`task` from 0.24.0) | a `Task` row holding the notice's status line, or its `Title:` text when it has none |
| `hook_result` | a `Hook` row, `<event>: <output>`; a blocking hook's output is an assistant message, and its row reads `<event> blocked: <output>` |
| `skill_activation` | `/<skill> <args>` under `You` when the user ran the skill; skipped when the model or another skill loaded it: its `Skill` call shows the load |
| `plugin_command` | `/<plugin>:<command> <args>` under `You` |
| `cron_job`, `cron_missed` | a `Scheduled` row: the reminder's prompt, or the count of missed reminders |
| `shell_command` | a shell command the user ran, with its output as the result |
| `injection`, `system_trigger`, `retry` | skipped: injected context, sub-agent prompts, goal continuations and empty retry messages |
| any other kind | the text under `You`, counted as unrecognised |

| Operation               | Behavior                                                                                                                                                        |
|-------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------|
| Resume                  | `kimi --session <session-id>`, in the session project directory                                                                                                 |
| Fork                    | Not Capable; `/fork` exists only inside a running session                                                                                                       |
| `[resume].default_args` | ignored                                                                                                                                                         |
| Rename                  | rewrites the title in `state.json`, preserving unknown fields                                                                                                   |
| Delete                  | removes the session directory, then its `session_index.jsonl` record; in a directory with no main wire, removes only the chosen wire while another wire remains |

### Upstream findings: Kimi

- **pending support** (0.2.0): images and videos attached to a user message are
  missing from the viewer. Kimi shows a placeholder for each.
- **pending support** (0.12.0): goals set with `/goal` and their status
  changes are missing from the viewer and search. Kimi shows each when it
  reopens a session and writes it as a `goal.create` or `goal.update`
  record.
  - Goals are on by default from 0.12.0.
- **deferred** (0.39.0): the viewer repeats the caller's user messages inside
  a sub-agent started with `fork`. Kimi copies the caller's conversation into
  the sub-agent's `wire.jsonl`. Deferred while sub-agent forking is
  experimental and off by default.

## Grok Build

| Method                                 | Value                                      |
|----------------------------------------|--------------------------------------------|
| `labels().name` / `.list` / `.display` | `grok` / `GROK` / `Grok`                   |
| `storage()`                            | `GrokStorage`                              |
| `format()`                             | `GROK_UPDATES`                             |
| `resume()` / `fork()`                  | `GrokLauncher`                             |
| `ref_namespaces().conversation`        | `agent-grok-v1`                            |
| `ref_namespaces().project`             | `agent-grok-project-v1`                    |
| `workspace_sessions_dir()`             | the cwd directory `--show-dir grok` prints |

| Storage               | Value                                                                     |
|-----------------------|---------------------------------------------------------------------------|
| Default root          | `~/.grok/sessions/`                                                       |
| Layout                | `<encoded-cwd>/<session-id>/updates.jsonl` beside `summary.json`          |
| Root override         | `GROK_HOME`, used as given when not empty                                 |
| Excluded files        | everything but each session directory's `updates.jsonl`                   |
| Session               | a session directory not named in any session's `subagents/*/meta.json`    |
| Sub-agent transcripts | the sessions those `meta.json` files name, nested ones after their parent |
| Sub-agent label       | the sub-agent type from its `summary.json`                                |
| External titles       | each session's `summary.json`                                             |
| Cache files           | `grok/root-<hash>/shard-NN.bin`                                           |
| Cache magic / schema  | `GKHIST01` / 6                                                            |

The cwd directory is the working directory URL-encoded while that is at most
255 bytes, else a slug of its last component and the first 16 hex digits of
its BLAKE3 hash, with the real path in a `.cwd` file. Discovery reads
neither, since `summary.json` states the working directory;
`workspace_sessions_dir` names the directory as Grok does.

A session directory with a summary and no `updates.jsonl` was closed before
its first prompt: its summary fingerprints it, and it reads as empty.
`external_titles` overlays the titles from `summary.json` on warm loads.

| Transcript format       | Value                                                                       |
|-------------------------|-----------------------------------------------------------------------------|
| Records                 | ACP update envelopes: `{timestamp, method, params: {update, _meta}}`        |
| Session header          | none in the file                                                            |
| File states its agent   | no; a sibling `summary.json` naming `info.id` is the ownership test         |
| Identity                | the session directory's name, a UUID v7                                     |
| Title / cwd / kind      | `summary.json`                                                              |
| Timestamps              | `params._meta.agentTimestampMs`, else the envelope's seconds; `0` is absent |

The reader is a projection. Adjacent chunk records of one kind become one
message when they share a prompt, a stream start and a prompt index. A call's
records (`tool_call`, then `tool_call_update` with its input, then one with
`status`) become one `ToolUse` and its result. The tool maps by
`_meta["x.ai/tool"].name`, then by that metadata's kind, then by a backend
call's `rawInput.variant`, then by the ACP kind. The result text comes from
`rawOutput` by its `type`; the type does not follow the tool's name, and two
output types store bytes as JSON number arrays. `turn_completed` carries the
model and tokens; `session_recap` is a searchable `Recap` row behind the
thinking toggle. A
`subagent_finished` record with any status but `completed` becomes a
searchable `Sub-agent` metadata row naming the sub-agent's type from its
`subagent_spawned` record, the status and, for a failure, the error; a
completed sub-agent's record is skipped, since its output repeats the
thread's last reply. Plans, task lifecycle, compaction, retries and memory
records are skipped: other records already carry them, or they are runtime
state.

| Operation               | Behavior                                                                                                         |
|-------------------------|------------------------------------------------------------------------------------------------------------------|
| Resume                  | `grok --resume <session-id>`, in the session project directory                                                   |
| Fork                    | `grok --resume <session-id> --fork-session`, in the current directory                                            |
| `[resume].default_args` | ignored                                                                                                          |
| Rename                  | sets `generated_title` and `title_is_manual` in `summary.json`, preserving unknown fields, then writes `title_refresh_idx` so Grok's auto title cannot overwrite it |
| Delete                  | removes every sub-agent session's directory, nested ones first, then the session directory, then their rows in `session_search.sqlite`, best-effort |

### Upstream findings: Grok

None recorded.

## Sub-agent sessions

Discovery reports each session as a `SessionStub` whose `subagents` field
names its sub-agent transcripts, nested ones flattened, and `parse_session`
merges them into the row: their text into `agent_search_text`, their messages and
tokens into the counts. A sub-agent transcript never becomes a row. Without
this, a session with sub-agents would list as several rows: the session the
user started, plus one per sub-agent transcript.

Each agent's Storage table names what counts as a session and where its
sub-agent transcripts are.

A chain of parents that loops back on itself has no session at its end; its
first ID in order lists as one, with the rest beneath it. A session no row
lists is a session the user cannot open.

The cache entry's fingerprint spans the session and its sub-agent transcripts
(`Fingerprint::spanning`: sizes summed, newest `modified`), so a sub-agent that
grows after the parent's last write invalidates the entry. The entry stores the
sub-agent paths (`ListedSessionEntry::subagents`), so a cache hit carries them
without a lookup.

Sub-agent text reaches `agent_search_text` only. It stays out of `full_text`, so the
TUI list does not search it. This matches how Claude already treats its own sub-agent
turns.

`agent search` and `agent read` read a tool call as its name and input keys. A
call that launches a sub-agent (`Tool::Agent`) adds its whole launch prompt,
the body the viewer shows for it. Each agent with sub-agents maps its
launching tool to `Tool::Agent`, except OMP: its `task` calls show their input
as JSON. A Codex `spawn_agent` message is read only when Codex did not encrypt
it.

The viewer, export and agent CLI read the sub-agent paths from the row, then
read the session through `format::view_projection`, which parses each sub-agent
transcript with the session's format and splices its turns into the entry
stream as `Progress` entries ordered by timestamp, the record older Claude Code
versions wrote for a sub-agent turn. Each thread is labelled with its header
ID, unless its agent's Storage table names a sub-agent label. A bare file
(`--render`, a direct path) reads the sub-agent transcripts its agent's
`SubagentFinder` names when its agent stores the file; a copy outside the
agent's tree shows its own content alone.

The row reads each sub-agent transcript once, with or without user text: its
text, tokens, `has_transient_read_error` and `is_missing_earlier_turns` join
the row, and its turns are numbered where the view splices them in. The row's
message count and a semantic hit's message number match `agent read`'s; both
count a sub-agent reply written as several records once.

`resolve_session_id` answers with the stub discovery would report, under its
root, so a session opened by ID runs the same cache-or-parse step as the list
and is the same row. Where a provider declares `subagent_by_id` Capable, a
sub-agent transcript's ID resolves to a stub of its own with the sub-agents
beneath it — the one exception to every filter the list applies.

## OpenCode

| Method                                 | Value                        |
|----------------------------------------|------------------------------|
| `labels().name` / `.list` / `.display` | `opencode` / `OC` / `OpenCode` |
| `storage()`                            | `OpenCodeStorage`            |
| `format()`                             | `OPENCODE_DB`                |
| `resume()` / `fork()`                  | `OpenCodeLauncher`           |
| `ref_namespaces().conversation`        | `agent-opencode-v1`          |
| `ref_namespaces().project`             | `agent-opencode-project-v1`  |

| Storage              | Value                                                                                   |
|----------------------|-----------------------------------------------------------------------------------------|
| Default root         | `$XDG_DATA_HOME/opencode/opencode.db` (`~/.local/share/opencode/opencode.db`)           |
| Layout               | one SQLite database; sessions are rows, not files                                       |
| Root override        | `OPENCODE_DB` (absolute, or joined under the data dir)                                  |
| Excluded sessions    | none                                                                                    |
| Session              | a `session_v2` or `session` row whose `parent_id` is null, or names an ID with no row   |
| Sub-agent sessions   | the rows beneath it by `parent_id`                                                      |
| External titles      | none needed; the title is part of the fingerprint                                       |
| Cache files          | `opencode/root-<hash>/shard-NN.bin`                                                     |
| Cache magic / schema | `OCHIST01` / 12                                                                         |

OpenCode 1 keeps a session in `session`, with `message` and `part` rows.
OpenCode 2 keeps it in `session_v2`, with `session_message` rows. A session in
both tables reads from `session_v2`, and rename and delete act on both rows:
when OpenCode 2 upgrades a database it copies each OpenCode 1 session into
`session_v2` under the same ID and writes new activity only to the copy.

There is no transcript file. A session's locator is
`<database-file>/<session-id>.jsonl` — the ID as a component under the
database file itself — and only this provider interprets it. Discovery
fingerprints every session in one query: the payload bytes of its message
rows (and, for OpenCode 1, part rows) plus the title's length, and the newest
`time_updated` among the session and its rows. A rename to a different-length
title always invalidates the cache; a rename by OpenCode itself to a
same-length title only reaches a warm cache when OpenCode also touches
`time_updated`.

| Transcript format       | Value                                                                                                                    |
|-------------------------|--------------------------------------------------------------------------------------------------------------------------|
| Records                 | OpenCode 1: `session`, `message`, `part` rows; OpenCode 2: `session_v2`, `session_message` rows; `data` columns are JSON |
| Identity                | the `ses_…` session row ID                                                                                               |
| Title / cwd / parentage | the session row's `title`, `directory`, `parent_id`                                                                      |
| Title kind              | autogenerated, no custom-title marker; projects as a title                                                               |
| Compaction              | OpenCode 1 `compaction` parts and completed OpenCode 2 `compaction` messages become invisible metadata                   |

The reader is a projection. In OpenCode 1, `text`, `reasoning` and `tool` parts
carry the conversation, `step-finish` parts carry tokens (message- and
session-level totals restate them and are not counted), and everything else —
`step-start` framing, `file` and `patch` attachments, snapshots, the
`session_input` event queue — is skipped without being an error.

OpenCode 2 writes one typed `session_message` row per message, read in `seq`
order. `user` text and the `text`, `reasoning` and `tool` items of `assistant`
messages carry the conversation; each assistant message's `tokens` carry usage,
since OpenCode 2 writes one assistant message per model request. A file
attached to a user message (an `@` mention) adds one line to that turn,
`[Attached <mime>: <name>]`, as OpenCode 2 writes it when it summarises a
session; its contents, stored inline as base64, are not shown. A `shell`
message is a command the user ran and projects as one. `system`, `idle`,
`skill` and the `agent-switched`, `model-switched` and `location-switched`
markers are skipped. OpenCode 2 renamed `bash`, `task` and `apply_patch` to
`shell`, `subagent` and `patch`, and its file tools take `path` where OpenCode
1's take `filePath`; a migrated session keeps OpenCode 1's names, and both map
to the same tools.

Parts flagged `synthetic` are OpenCode's prompt plumbing, not the user's
words. The ones that narrate an injected read (`@`-file mentions, MCP
resources) project as a `read` tool call with the injected contents as its
result, so they follow the tools toggle and index bounded like real tool
output. The rest (reminders, editor context, comment notes) yields nothing.
OpenCode 2 joins a migrated message's synthetic parts with a blank line into
one `synthetic` message, which projects the same way. The narration prefixes
are OpenCode's own literals; if a newer OpenCode rewords them, the run
degrades to skipped, never to inline text. A sub-agent is a `session_v2` or
`session` row whose `parent_id` names its parent; discovery names it on the
parent's stub, `parse_session` merges it into the parent's list row, and the
view splices it. Queries name their columns explicitly, because the schema
migrates freely between OpenCode versions. Channel-specific databases
(`opencode-<channel>.db`) are not read.

OpenCode journals each migration it applies inside the database. The reader
pins the newest one it was developed against (`NEWEST_VERIFIED_MIGRATION` in
`src/history/format/opencode.rs`); a database migrated beyond the pin logs a
warning under `--debug` rather than failing, since a dropped or renamed
column already fails its query loudly. The journal records schema changes
only — the JSON payloads inside `data` columns can change shape without a
migration, and no check detects that.

| Operation               | Behavior                                                                                                                                |
|-------------------------|-----------------------------------------------------------------------------------------------------------------------------------------|
| Resume                  | `opencode --session <session-id>`, in the session project directory                                                                     |
| Fork                    | `opencode --session <session-id> --fork`, in the current directory; Not Capable with OpenCode 2 installed                               |
| `[resume].default_args` | ignored                                                                                                                                 |
| Rename                  | updates `title` and `time_updated` in each table holding the session                                                                    |
| Delete                  | deletes the session's rows and its sub-agent sessions' rows, with their messages; leaves OpenCode 2's event log (`event` rows) as it is |

Reads open the database read-only with a 5000 ms busy timeout — the setting
OpenCode itself runs with — so a live OpenCode holding the WAL writer does not
fail the load. A present database that is locked past that wait, cannot be
opened, or fails a query stops the OpenCode load for that launch, and
session-ID lookup and delete fail with the same reason, reported as Codex's
is (the first three rows of Codex's reason table, under `OpenCode`). Rename
and delete open read-write with foreign keys on. A sub-agent session's
`parent_id` has no foreign key, so delete finds the sub-agent sessions itself
and removes them with the session in one transaction.

### Upstream findings: OpenCode

None recorded.

## Add a provider

Write the provider, then let the compiler and `just check` name what is
missing.

1. Add a variant to `Source` and create `src/history/provider/<agent>.rs`
   with a unit struct implementing `SessionProvider`. Register its `static`
   in `PROVIDERS`. `<agent>` is the variant in lowercase or snake_case.
2. Implement `SessionStorage`, returning the same `Source` from `storage()`
   as from the provider. Mark a root the agent installs itself with
   `SessionRoot::in_agent_tree()`. `discover()` reports
   each session as a `SessionStub` (locator, cache key, change fingerprint),
   and the load loop never stats or opens a locator itself, so a root does
   not have to be a directory of transcript files. File-backed providers
   compose the walkers in `provider/walk.rs`.
3. Declare each capability [Capabilities](#capabilities) lists. Where a
   Capable declaration carries a trait, implement the trait on the
   provider's unit struct and return `Capability::Capable(self)`, or on a
   launcher struct of the provider's own for `resume` and `fork`. Declare Not
   Capable as `Capability::NotCapable(None)`, or with a `DenialDetail` when
   the user needs guidance once the app refuses. Where the `fork` capability
   method reads anything, override `fork_if_known` too.
4. Implement `SessionFormat` in `src/history/format/<agent>.rs`.
   `parse_transcript` recognizes the agent's transcripts and projects them;
   the default `parse_conversation` and `session_entries` build the row and
   the view from that projection. Override them only when the agent's records
   need another reading, as Claude's do.
5. Set `tool` on every `ContentBlock::ToolUse` the format builds: map the
   agent's tool names onto `Tool` and reshape the input to the canonical keys
   (`command`, `file_path`, …). Summary mode buckets on `tool` and tool
   headers lay out the input by it while printing the agent's own `name`. A
   block left at `Other` counts as "called N tools" and renders as its name
   plus raw input; no test catches it.
6. Implement `ProviderFixture` in the provider's test module, including an
   `every_record_session` with every record type the agent writes. The
   provider's capability declarations ([Capabilities](#capabilities)) decide
   which contracts run for it. The fixture writes what each of those contracts
   needs, such as a sub-agent where `subagents` is Capable. It states the
   fork command line and directory it expects where `fork` is Capable. Where
   `workspace_sessions_dir` is Capable, `session_working_dir` names a working
   directory whose sessions directory `write_session` writes into, and
   `root_variables` points the provider's root at the fixture's. Its own
   test module keeps only agent-specific tests.
7. Run `just check` and fix what it names:

   | Failure | Add |
   |---------|-----|
   | A compile error for a missing method or `Source` arm | The method, or the arm |
   | `registry_lists_every_source_exactly_once` | The provider's `static` in `PROVIDERS` |
   | `registry_and_source_lookup_agree` | The `Source` arm in `Source::provider()` that returns this provider |
   | `storage_collects_the_sessions_of_the_provider_that_offers_it` | The provider's own `Source` from its storage |
   | `labels_are_unique` | Labels no other provider uses |
   | `every_registered_provider_has_one_fixture` | The fixture in `FIXTURES` in `provider/contract_tests.rs` |
   | A contract naming the provider | A fix, or a corrected capability declaration |
   | `subagent_by_id_is_capable_only_where_subagents_and_session_id_lookup_are` | `subagent_by_id` declared Not Capable, or `subagents` and `session_id_lookup` declared Capable |
   | `the_providers_md_capability_table_is_up_to_date` | The table `mise run update-capability-table` writes |
   | `ref_namespaces_are_unique_across_providers` | Reference namespaces no other provider uses |
   | `session_caches_do_not_share_a_directory_or_magic` | A cache directory and magic no other provider uses |
   | `session_cache_identities_are_pinned` | The provider's cache directory, magic and schema version |
   | Clippy `disallowed_methods` under `src/history` | Environment reads through `RootEnvironment` in `roots_in`, each variable listed in `storage_env_vars` |
   | `readme_and_crate_description_list_every_registered_agent` | The agent's row in the README's `Supported agents` table, with its list label, and the row's Agent name in the README introduction and the crate description |
   | `providers_md_records_every_registered_agent_consistently` | The agent's row in `## Upstream releases`, with the release the provider was written against in both version columns and today's date as "Last review", and its `### Upstream findings: <agent>` subsection |
   | `every_registered_agent_has_its_provider_releases_file` | Its file in `workflows/provider-releases/`, named `<labels().name>.md` |

8. Add what no test checks:
   - a section of its own in this file, with the same tables as the other
     sections, its Storage table naming its sessions, sub-agent transcripts
     and any sub-agent label;
   - a section of its own under the README's `Agent history search
     locations`;
   - a CHANGELOG line.
