# Glossary

One word for each concept, in the interface, docs, comments, test names and
identifiers.

| Term | Meaning | Replaces |
|---|---|---|
| agent | A coding agent whose sessions rearview reads, such as Claude Code or Codex. In code, an agent's provider reads its sessions; see [PROVIDERS.md](PROVIDERS.md). | |
| collapsed | Shown as one row or line that stands for more. The viewer shows a collapsed tool run as one row. `agent read` of several messages prints a collapsed tool call: the call's line, plus `(+N lines)` for the lines it leaves out. Expanding the run, or reading the call's message alone, shows the rest. | |
| ignored | Skipped and reported: `Codex │ 12 ignored: compressed sessions unsupported`. | not read, unread |
| limit | The most a count, size or length may reach: a line limit, a budget. | cap, bound |
| low space | A cache's drive holding less free space than the 1 GB a write to the session cache or the passage store needs; `low_space` in identifiers. The session cache stops and is deleted; the passage store skips the write. "Low on space" is its predicate form: the drive is low on space. | short of space |
| nested sub-agent | A sub-agent's own sub-agent. | grandchild |
| passage store | The SQLite file in the semantic cache folder that holds each passage's embedding, one row per passage; `PassageStore` in code. | embedding cache (kept in the lines `--debug-semantic-search` prints) |
| permanent error | Any error that is not a transient error. | |
| session | One conversation with an agent, as the list shows it. | rollout (kept in code as the name of Codex's transcript file) |
| session ID | The identifier an agent gives a session, such as a UUID or OpenCode's `ses_…`. Written `ID` in prose; field names and flags keep their spelling (`id`, `--show-id`). | id |
| stamp | The day a passage's row in the passage store was last written or read, as the store records it. A read within 7 days of the stamp leaves it as it is. | |
| sub-agent | A session or thread an agent ran under another session; `subagent` in identifiers. | child, descendant, spawned |
| transcript | The record an agent writes for a session: a file, or for OpenCode the session's rows in its database. | |
| transient error | An error that can clear on its own: a lock, a busy database, interrupted I/O. | temporary error, locked |
| truncated | Shortened to a limit: a tool result to its line limit, an `agent read` message to its `--budget`. The viewer's `tools·trn` mode truncates tool results. | cut, capped, clipped, abbreviated |
