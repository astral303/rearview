# Provider support

A provider is one coding agent whose history this tool reads. `Source` identifies
the agent. `SessionProvider` holds everything that differs between agents.

To add an agent, register a provider. Do not add match arms.

```rust
Source::Pi.provider().launcher()   // reach behavior through the registry
provider::providers()              // iterate every registered provider
```

Code lives in `src/history/provider/` (policy) and `src/history/format/` (wire
formats).

The rules the registry enforces for every provider come first. Each provider then
has a section of its own; Pi and OMP share one because they share a wire format.

## Registry contract

`storage` and `format` return `Option`. A provider returns `None` when it does not
have that capability. The other methods are mandatory.

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

A provider whose `max_session_bytes()` returns a limit makes the load loop skip
larger transcripts and log a warning. Every registered provider returns `None`:
no transcript is skipped for size.

`[resume].default_args` documents arguments for the `claude` CLI.
`SessionLaunch::configured_args` carries them to every launcher, and only Claude's
applies them.

## Claude

| Capability                             | Value                      |
|----------------------------------------|----------------------------|
| `labels().name` / `.list` / `.display` | `claude` / `CC` / `Claude` |
| `storage()`                            | `None`                     |
| `format()`                             | `None`                     |
| `launcher()`                           | `ClaudeLauncher`           |
| `ref_namespaces().conversation`        | `None`                     |
| `ref_namespaces().project`             | `agent-project-v1`         |

The conversation namespace is `None` because Claude references predate per-source
digests; they derive from the project directory and session filename.

Claude returns `None` from both optional capabilities, for two different reasons:

- **`storage()`** — Claude partitions sessions by project directory, not by session
  root. It excludes `agent-*.jsonl`, caches per project, and streams project batches
  to the TUI.
- **`format()`** — Claude writes `LogEntry` records with no session header. There is
  no id, start time, or cwd to project, and entries chain linearly. A file that no
  format claims is read as a Claude transcript. The one projection Claude needs,
  the canonical `Tool` of each tool call, is `assign_canonical_tools` in
  `provider/claude.rs`, applied to every record after deserializing.

| Storage              | Value                               |
|----------------------|-------------------------------------|
| Default root         | `~/.claude/projects/<encoded-cwd>/` |
| Layout               | one directory per project           |
| Root override        | `CLAUDE_CONFIG_DIR`                 |
| Excluded files       | `agent-*.jsonl`                     |
| Cache file           | `projects/<name>.bin`               |
| Cache magic / schema | `CLHIST01` / 11                     |

| Operation               | Behavior                                                                       |
|-------------------------|--------------------------------------------------------------------------------|
| Resume                  | `claude --resume <id>`, in the directory that maps to the transcript's project |
| Fork                    | `claude --resume <id> --fork-session`, in the current directory                |
| Cross-project fork      | copies transcript and session directory                                        |
| `[resume].default_args` | applied                                                                        |
| Rename                  | appends `custom-title` and `agent-name` records                                |
| Delete                  | removes every copy, by session id                                              |

Claude resumes by id and finds the transcript through the directory it runs in. When
the session sits under a project Claude does not search, the launcher copies the files
to one it does. Building a Claude command therefore writes to disk. This is why a
launcher returns a command instead of running one.

## Pi and OMP

Pi and OMP share one wire format. Two `PiLogFormat` statics read it. They differ only
in the source they attribute a transcript to.

| Capability                             | Pi                    | OMP                    |
|----------------------------------------|-----------------------|------------------------|
| `labels().name` / `.list` / `.display` | `pi` / `Pi` / `Pi`    | `omp` / `OMP` / `OMP`  |
| `storage()`                            | `PiStorage`           | `OmpStorage`           |
| `format()`                             | `PI_LOG`              | `OMP_LOG`              |
| `launcher()`                           | `PathResumeLauncher`  | `PathResumeLauncher`   |
| `ref_namespaces().conversation`        | `agent-pi-v1`         | `agent-omp-v1`         |
| `ref_namespaces().project`             | `agent-pi-project-v1` | `agent-omp-project-v1` |

| Storage              | Pi                                                                                    | OMP                                                                            |
|----------------------|---------------------------------------------------------------------------------------|--------------------------------------------------------------------------------|
| Default root         | `~/.pi/agent/sessions/`                                                               | `~/.omp/agent/sessions/`                                                       |
| Layout               | child directories                                                                     | child directories                                                              |
| Root overrides       | `PI_CODING_AGENT_DIR`, `PI_CODING_AGENT_SESSION_DIR`, `sessionDir` in `settings.json` | same three, plus `PI_CONFIG_DIR`, `OMP_PROFILE`, `PI_PROFILE`, `XDG_DATA_HOME` |
| Override layout      | flat root                                                                             | flat root                                                                      |
| Excluded files       | none                                                                                  | none                                                                           |
| Cache file           | `pi/root-<hash>/sessions.bin`                                                         | `omp/root-<hash>/sessions.bin`                                                 |
| Cache magic / schema | `PIHIST01` / 2                                                                        | `OMHIST01` / 2                                                                 |

| Transcript format             | Pi                                            | OMP                                           |
|-------------------------------|-----------------------------------------------|-----------------------------------------------|
| Records                       | Pi log JSONL                                  | Pi log JSONL                                  |
| Session header                | `{"type":"session", ...}`                     | same, after `title` slot                      |
| Header fields                 | `id`, `timestamp`, `cwd`, `version` 1–3       | same                                          |
| File states its agent         | no                                            | yes, through `title` slot                     |
| Agent when the file is silent | Pi                                            | OMP in OMP's own tree, Pi in a redirected one |
| Branching                     | `parentId` chain; only active branch projects | same                                          |

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

| Operation               | Pi                            | OMP                                           |
|-------------------------|-------------------------------|-----------------------------------------------|
| Resume                  | `pi --session <path>`         | `omp --resume <path>`                         |
| Resume runs in          | session project directory     | session project directory                     |
| Fork                    | `pi --fork <path>`            | `omp --fork <path>`                           |
| Fork runs in            | current directory             | current directory                             |
| `[resume].default_args` | ignored                       | ignored                                       |
| Rename                  | appends `session_info` record | rewrites `title` slot, appends `title_change` |
| Delete                  | removes file                  | removes file and artifact directory           |

## Codex

| Capability                             | Value                     |
|----------------------------------------|---------------------------|
| `labels().name` / `.list` / `.display` | `codex` / `CDX` / `Codex` |
| `storage()`                            | `CodexStorage`            |
| `format()`                             | `CODEX_ROLLOUT`           |
| `launcher()`                           | `CodexLauncher`           |
| `ref_namespaces().conversation`        | `agent-codex-v1`          |
| `ref_namespaces().project`             | `agent-codex-project-v1`  |

| Storage              | Value                                                         |
|----------------------|---------------------------------------------------------------|
| Default root         | `~/.codex/sessions/`                                          |
| Layout               | dated tree: `<YYYY>/<MM>/<DD>/rollout-<stamp>-<thread>.jsonl` |
| Root override        | `CODEX_HOME` (moves the whole home)                           |
| Excluded files       | files not named as rollouts; superseded rollouts of a thread  |
| External titles      | `session_index.jsonl`, beside the sessions tree               |
| Cache file           | `codex/root-<hash>/sessions.bin`                              |
| Cache magic / schema | `CXHIST01` / 6                                                |

An undo leaves several rollouts of one thread on disk; discovery lists only the
newest, the file Codex itself resumes. Thread titles live in `session_index.jsonl`,
so a rename never touches the rollout the cache validates against;
`external_titles` overlays them on warm loads.

| Transcript format     | Value                                                                          |
|-----------------------|--------------------------------------------------------------------------------|
| Records               | rollout JSONL: `{{timestamp, ordinal?, type, payload}}`                        |
| Session header        | the first `session_meta` line                                                  |
| Header fields         | `id`, `timestamp`, `cwd`, `parent_thread_id`, `subagent_history_start_ordinal` |
| File states its agent | yes, through the header                                                        |
| Title                 | newest `session_index.jsonl` record naming the thread                          |
| Sub-agent threads     | one rollout per thread, linked by `parent_thread_id`                           |
| Compaction            | `compacted` events become invisible metadata                                   |

The reader is a projection: `response_item` lines carry the dialogue, `token_count`
events carry usage, `turn_context` names the model, and everything else — context
snapshots, encrypted reasoning, telemetry — is skipped without being an error. A
sub-agent rollout restates the parent's history below
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

Delete leaves Codex's state databases (`state_5.sqlite`, `thread_history_1.sqlite`)
alone. Codex drops a thread whose rollout is missing from its own list, so the
rows left behind cost disk, not behaviour, and the version-suffixed schema is
Codex's to migrate.

## Kimi Code

| Capability                             | Value                    |
|----------------------------------------|--------------------------|
| `labels().name` / `.list` / `.display` | `kimi` / `KIMI` / `Kimi` |
| `storage()`                            | `KimiStorage`            |
| `format()`                             | `KIMI_WIRE`              |
| `launcher()`                           | `KimiLauncher`           |
| `ref_namespaces().conversation`        | `agent-kimi-v1`          |
| `ref_namespaces().project`             | `agent-kimi-project-v1`  |

| Storage              | Value                                                   |
|----------------------|---------------------------------------------------------|
| Default roots        | `~/.kimi-code/sessions/` and legacy `~/.kimi/sessions/` |
| Layout               | `<workspace>/<session>/agents/<agent>/wire.jsonl`       |
| Root override        | `KIMI_CODE_HOME` (replaces both defaults)               |
| Excluded files       | everything not named `wire.jsonl`                       |
| External titles      | each session's `state.json`                             |
| Cache file           | `kimi/root-<hash>/sessions.bin`                         |
| Cache magic / schema | `KIHIST01` / 2                                          |

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
| Title / cwd / parentage | the session's `state.json`                                     |
| Compaction              | `context.apply_compaction` becomes invisible metadata          |

The reader is a projection: `context.append_message` carries the user's turns,
loop events carry the assistant's text and tool traffic, turn-scoped
`usage.record` events carry the model and tokens, and everything else — LLM
request payloads, permission prompts — is skipped without being an error.
`turn.prompt` and `turn.steer` restate appended messages and are not read twice;
session-scoped `usage.record` events restate running totals and are not counted.

| Operation               | Behavior                                                             |
|-------------------------|----------------------------------------------------------------------|
| Resume                  | `kimi --session <session-id>`, in the session project directory      |
| Fork                    | refused; `/fork` exists only inside a running session                |
| `[resume].default_args` | ignored                                                              |
| Rename                  | rewrites the title in `state.json`, preserving unknown fields        |
| Delete                  | removes the session directory, then its `session_index.jsonl` record |

## Sub-agent sessions

An agent that writes each sub-agent thread to its own transcript file sets
`SessionProjection::parent_session_id`. The load loop then folds that session into
its parent: the child's searchable text, message count and tokens join the parent's,
and the child does not become a row. Without the fold, a session that spawned
sub-agents would list as several rows: the session the user started, plus one
per sub-agent transcript.

Codex and Kimi both set it: Codex records each sub-agent thread as a rollout of
its own, Kimi as a wire per agent inside the session directory. (Claude keeps
sub-agent turns inside the parent transcript as `LogEntry::Progress` entries; Pi
and OMP do not record sub-agent turns.)

A thread folds into the far end of its chain of parents, not its immediate one, so
a sub-agent that spawned a sub-agent still lands on the session the user started.

Every session that cannot be resolved that far keeps its own row:

| Case                        | Result                                  |
|-----------------------------|-----------------------------------------|
| Parent absent from the load | child stays a row                       |
| Chain loops back on itself  | every session in the loop stays a row   |
| One session id on two rows  | both stay; threads fold into the first  |

Nothing is dropped without being merged somewhere. A session no row lists is a
session the user cannot open.

`parent_session_id` is cached, so a folded session stays folded on a cache hit.
Cache entries hold the session as parsed, one per transcript. Folding runs after
every root is loaded and is redone on each load, so a folded count must never
reach the cache — it would be added to again on the next run.

Sub-agent text reaches `agent_search_text` only. It stays out of `full_text`, so the
TUI list does not search it. This matches how Claude already treats its own sub-agent
turns.

Folding hides the child's row; splicing is what renders it nested in the viewer.
The viewer, export and agent CLI read a session through
`SessionFormat::parse_transcript_view`, which splices the threads spawned from it
into the entry stream as `Progress` entries ordered by timestamp — the shape
Claude writes natively. The bulk loader calls `parse_transcript` instead and
folds whole conversations, so loading a corpus stays linear in the number of
files.

## OpenCode

| Capability                             | Value                        |
|----------------------------------------|------------------------------|
| `labels().name` / `.list` / `.display` | `opencode` / `OC` / `OpenCode` |
| `storage()`                            | `OpenCodeStorage`            |
| `format()`                             | `OPENCODE_DB`                |
| `launcher()`                           | `OpenCodeLauncher`           |
| `ref_namespaces().conversation`        | `agent-opencode-v1`          |
| `ref_namespaces().project`             | `agent-opencode-project-v1`  |

| Storage              | Value                                                  |
|----------------------|--------------------------------------------------------|
| Default root         | `$XDG_DATA_HOME/opencode/opencode.db` (`~/.local/share/opencode/opencode.db`) |
| Layout               | one SQLite database; sessions are rows, not files      |
| Root override        | `OPENCODE_DB` (absolute, or joined under the data dir) |
| Excluded sessions    | `time_archived IS NOT NULL`                            |
| External titles      | none needed; the title is part of the fingerprint      |
| Cache file           | `opencode/root-<hash>/sessions.bin`                    |
| Cache magic / schema | `OCHIST01` / 1                                         |

There is no transcript file. A session's locator is
`<database-file>/<session-id>.jsonl` — the id as a component under the
database file itself — and only this provider interprets it. Discovery
fingerprints every session in one query: the payload bytes of its message and
part rows plus the title's length, and the newest `time_updated` among the
session and its rows. A rename to a different-length title always invalidates
the cache; a rename by OpenCode itself to a same-length title only reaches a
warm cache when OpenCode also touches `time_updated`.

| Transcript format       | Value                                                       |
|-------------------------|-------------------------------------------------------------|
| Records                 | `session`, `message`, and `part` rows; `data` columns are JSON |
| Identity                | the `ses_…` session row id                                  |
| Title / cwd / parentage | the session row's `title`, `directory`, `parent_id`         |
| Title kind              | autogenerated, no custom-title marker; projects as a title  |
| Compaction              | `compaction` parts become invisible metadata                |

The reader is a projection: `text`, `reasoning` and `tool` parts carry the
conversation, `step-finish` parts carry tokens (message- and session-level
totals restate them and are not counted), and everything else — `step-start`
framing, `file` and `patch` attachments, snapshots, the `session_input` event
queue — is skipped without being an error.

Parts flagged `synthetic` are OpenCode's prompt plumbing, not the user's
words. The ones that narrate an injected read (`@`-file mentions, MCP
resources) project as a `read` tool call with the injected contents as its
result, so they follow the tools toggle and index bounded like real tool
output. The rest (reminders, editor context, comment notes) yields nothing.
The narration prefixes are OpenCode's own literals; if a newer OpenCode
rewords them, the run degrades to skipped, never to inline text. Sub-agents are child sessions
(`parent_id`), folded at load and spliced into the parent's view. Queries name
their columns explicitly, because the schema migrates freely between OpenCode
versions. Channel-specific databases (`opencode-<channel>.db`) are not read.

OpenCode journals each migration it applies inside the database. The reader
pins the newest one it was verified against (`NEWEST_VERIFIED_MIGRATION` in
`src/history/format/opencode.rs`); a database migrated beyond the pin logs a
warning under `--debug` rather than failing, since a dropped or renamed
column already fails its query loudly. The journal records schema changes
only — the JSON payloads inside `data` columns can change shape without a
migration, and no check detects that.

| Operation               | Behavior                                                                             |
|-------------------------|--------------------------------------------------------------------------------------|
| Resume                  | `opencode --session <session-id>`, in the session project directory                  |
| Fork                    | `opencode --session <session-id> --fork`, in the current directory                   |
| `[resume].default_args` | ignored                                                                              |
| Rename                  | updates the session row's `title` and `time_updated`                                 |
| Delete                  | deletes the session row and its sub-agent sessions' rows; messages and parts cascade |

Reads open the database read-only with a 5000 ms busy timeout — the setting
OpenCode itself runs with — so a live OpenCode holding the WAL writer does not
fail the load. Rename and delete open read-write with foreign keys on. A
sub-agent session's `parent_id` has no foreign key, so delete finds the
sub-agent sessions itself and removes them with the session in one
transaction.

## Add a provider

1. Add a variant to `Source`. Fix every exhaustiveness error the compiler reports.
2. Create `src/history/provider/<agent>.rs`. Declare a unit struct in it and
   implement `SessionProvider` for that struct.
3. Declare the module in `provider/mod.rs`. Add a `static` for the struct, list that
   static in `PROVIDERS`, and map the new `Source` variant to it in
   `Source::provider()`.
4. Return `RefNamespaces` values that no other provider uses.
5. Implement `SessionStorage` if the agent keeps sessions under roots. Otherwise
   return `None` from `storage()`. Return the same `Source` from `storage()` as
   from the provider, and mark a root the agent installs itself with
   `SessionRoot::in_agent_tree()`. `discover()` reports each session as a
   `SessionStub` — locator, cache key, change fingerprint — and the load loop
   consumes stubs as given — it never stats or opens a locator itself — so a
   root does not have to be a directory of transcript files. File-backed providers
   compose the walkers in `provider/walk.rs`.
6. Implement `SessionFormat` in `src/history/format/` if transcripts carry a session
   header. Otherwise return `None` from `format()`.
7. Set `tool` on every `ContentBlock::ToolUse` the format builds: map the agent's
   tool names onto `Tool` and reshape the input to the canonical keys (`command`,
   `file_path`, …). Summary mode buckets on `tool` and tool headers lay out the
   input by it while printing the agent's own `name`. A block left at `Other`
   counts as "called N tools" and renders as its name plus raw input.
8. Document the provider in a section of its own in this file, with the same
   tables as the sections above.

Nothing else needs a change. Every other consumer reads the registry.

Run `just check`. The tests in `src/history/provider/mod.rs` enforce the registry
invariants: one entry per source, unique labels, unique reference namespaces, and
unique cache identities.
