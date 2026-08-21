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

Caches live under `~/.cache/claude-history/`, or under the directory
`CLAUDE_HISTORY_CACHE_DIR` names. `SessionCacheStore` holds the
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
  format claims is read as a Claude transcript.

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
| Cache magic / schema | `PIHIST01` / 1                                                                        | `OMHIST01` / 1                                                                 |

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
7. Document the provider in a section of its own in this file, with the same
   tables as the sections above.

Nothing else needs a change. Every other consumer reads the registry.

Run `just check`. The tests in `src/history/provider/mod.rs` enforce the registry
invariants: one entry per source, unique labels, unique reference namespaces, and
unique cache identities.
