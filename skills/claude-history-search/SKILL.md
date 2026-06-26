# claude-history-search

Use this skill when you need to find, narrow, or quote prior Claude Code conversation context with `claude-history`.

## Workflow

Start with the search mode that matches the task. For conceptual recall, prefer semantic or hybrid:

```sh
claude-history agent search --hybrid "deployment rollback decision" --top 5
claude-history agent search --semantic "why the cache invalidation approach changed" --top 5
```

For exact terms, identifiers, filenames, commands, error messages, or stack traces, use lexical or exact:

```sh
claude-history agent search --lexical "auth cache bug"
claude-history agent search --exact "DEPLOYMENT_TOKEN"
```

The output is protocol text, not JSON. Global search is grouped by conversation, with readable snippets after `|` and copyable `read ref=... focus=...` lines:

```text
protocol agent-search v=2 mode=lexical groups=1 hits=1
query text=auth%20cache%20bug hits=1
groups count=1
conversation rank=1 uuid=12345678-1234-4234-9234-123456789abc ref=ch_1234abcd5678 score=12.500000 hits=1 total=1 | fix auth cache
hit uuid=12345678-1234-4234-9234-123456789abc ref=ch_1234abcd5678 source=lexical score=12.500000 focus=m8..m8 | auth cache bug repro
read ref=ch_1234abcd5678:m7..m9 focus=m8..m8
```

Copy the emitted `read ref=... focus=...` line as an instruction for the next command. Use `uuid=` when reporting the conversation ID to the user. Use only `ref=ch_...` or emitted `read ref=...` handles for `within`, `outline`, `read`, and qualified `--focus`. Do not use UUIDs as command refs. Do not treat hit order, scores, ranks, or chunks as stable addresses.

If the top hit is probably the right conversation but you need better evidence inside it, narrow first:

```sh
claude-history agent within ch_1234abcd5678 --lexical "auth cache bug"
```

If you need to choose a section before reading, outline the conversation:

```sh
claude-history agent outline ch_1234abcd5678
```

Then read only the emitted range and preserve `focus=` in `--focus`:

```sh
claude-history agent read ch_1234abcd5678:m7..m9 --focus m8..m8
```

Use one `agent read` command per emitted `read` line unless you qualify focus with the conversation ref, for example `--focus ch_1234abcd5678:m8..m8`. A bare `--focus m8..m8` is only unambiguous when reading one conversation.

Do not read a full transcript by default. Prefer `search`, then `within` or `outline`, then a bounded `read` range. Use `--flat` only when you need raw message-hit ordering, `--hits-per-conv` when you need more evidence from each conversation, and `--all-hits` only when duplicate suppression hides relevant tool-heavy evidence. Use `--tools`, `--tool-results`, `--thinking`, or `--subagents` only when that hidden content is relevant.

## Query mode guidance

Use `--semantic` when the user asks to find what was discussed, decided, designed, or debugged and the exact wording may differ. Use `--hybrid` when semantic recall is useful but concrete terms still matter, such as product names, technologies, or domain words.

Use `--lexical` for identifier-like terms such as `api_key`, `build_id`, or `AgentSearchRequest`. Use `--exact` or quoted text for exact tokens, secrets, IDs, error strings, and case-sensitive identifiers:

```sh
claude-history agent search --hybrid "deployment rollback decision" --top 5
claude-history agent search --semantic "why the cache invalidation approach changed" --top 5
claude-history agent search --exact "DEPLOYMENT_TOKEN"
claude-history agent within ch_1234abcd5678 --lexical "api_key"
```

After a broad semantic or hybrid search finds a likely conversation, use `within` with lexical, exact, semantic, or hybrid based on what evidence you need next. Lexical narrowing is often best when the global hit includes useful concrete terms.
