## Unreleased

### Enhancements

- Show that a slow agent command is still running: after 2 seconds, and every
  5 seconds until its result, it writes a `protocol agent-progress` record to
  stderr naming its stage, such as `stage=refresh done=120 total=860`.
  - While the command waits for another rearview's session cache refresh,
    the record adds `waiting=refresh-owner`.
- Show the session list at once while another rearview refreshes the session
  cache, under `Another rearview is refreshing…`. The list updates when that
  refresh completes.
  - While another rearview rebuilds the session cache, the list waits under
    `Another rearview is rebuilding the session cache…`.
- Open a changed session without waiting for it to be read again: the viewer
  shows its cached copy under `Updating…`, or `Loading…` for a session not
  yet cached, and shows its newest messages when its refresh completes.
  - Viewer `Ctrl+R` and a session found by its ID refresh the same way.
  - While another rearview refreshes or rebuilds the session cache, the viewer
    shows `Another rearview is refreshing…` or
    `Another rearview is rebuilding the session cache…` instead.
  - Message copy and export wait for that refresh, so they include the newest
    messages. If the refresh cannot read the session, export reports
    `Refresh failed: the session could not be read`.
- Warn when an agent command may miss messages because a session's cached copy
  is out of date or missing: a `protocol agent-warning stale-cache` record
  names those sessions' refs and, in `cached-until=`, the earliest time their
  copies end.
- Parse new and changed sessions in one rearview at a time: while one refreshes
  the session cache, the others read the cache and parse nothing.
- Add `--perf-stats` to every agent command, such as
  `agent read <ref> --perf-stats`. It reports on stderr each stage's time, what
  each agent's load read, each transcript parsed, and the peak memory.

### Fixes

#### All agents

- Fix one unreadable directory hiding all of an agent's sessions from the list
  and agent commands (Codex, Grok Build, Kimi Code, OMP and Pi). Its cached
  sessions now stay listed and readable, agent commands name the directory in
  an `io` warning, and `--debug` names it.

### Changes

- The first run after upgrading parses every session once to build the new
  session cache, then removes the old cache's files.
- rearview stops when it finds no directory for the session cache, naming
  `REARVIEW_CACHE_DIR`, instead of reading sessions without caching them. Agent
  commands report the same in a `protocol agent-error` record. `update`,
  `--render`, `--show-dir` and `--clear-semantic-cache` run without one.
- The `Ctrl+X` actions menu greys out rename and delete, with the reason, while
  the list loads or refreshes and while that session's refresh runs. Rename
  (`F2`) shows the reason. Both stay available while that work waits for
  another rearview's refresh.
- rearview deletes its session cache and stops when a cache write finds less
  than 1 GB free on its drive, naming the free space. Agent commands report the
  same in a `protocol agent-error` record.

### Internal: development

- Add DEVELOPMENT.md, with setup, checks, design principles and the
  conventions for contributions, and GLOSSARY.md, with one word for each
  concept.
- `cargo install` in README now passes `--locked`, to build with the
  dependency versions rearview was tested with.

## v0.6.1 (2026-10-26)

### Internal: releases

- Fix a regression preventing releases from building. No binaries were made
  available for v0.6.0.

## v0.6.0 (2026-10-26)

### Security

- Fix the semantic model download trusting certificates issued under
  eMudhra's compromised emSign intermediate certificate authorities
  ([RUSTSEC-2026-0381](https://rustsec.org/advisories/RUSTSEC-2026-0381.html),
  [RUSTSEC-2026-0382](https://rustsec.org/advisories/RUSTSEC-2026-0382.html)).

### Enhancements

- Add support for Grok Build sessions: list, search, read, resume, fork,
  rename and delete.
- Add support for OpenCode 2 sessions: list, search, read, resume, rename and
  delete.
  - A session made before upgrading to OpenCode 2 now shows its messages
    from after the upgrade too.
- Speed up semantic search (on one example corpus). Ported from
  claude-history v0.1.75.
  - ~14x per query in the session list
  - ~1.25x for semantic `agent search`; little for hybrid `agent search`
  - ~9x for the first semantic search after a refresh or a rename
- Print Kimi Code's, Grok Build's, Pi's or OMP's sessions directory for the
  current directory with `--show-dir <agent>`, and the sessions directory of
  each agent that has one with `--show-all-dirs`, one `<agent>: <path>` line
  each.
- Add `agent sessions` to list sessions without a search query, newest first.
  With `--since` or `--before`, it lists only the sessions with a message
  inside the window.
  - Each session shows its message count and the first and last time of its
    messages. With a window, the times are those of its messages inside it,
    and `messages-in-window=N` counts them.
- Read the end of a session in one command with `agent read <ref> --tail N`:
  the last N messages of each ref or range, inside any time window, not
  counting hidden messages.
  - When the last N messages exceed `--budget`, the read shows as many of the
    latest messages as fit, truncating a message too long to show whole. A
    `continue read` line lists the omitted messages.
- Accept a session ID or a transcript path in `agent read`, `agent outline`
  and `agent within`, not only a `ch_` handle. Ported from claude-history
  v0.1.75.
- Read or outline one sub-agent's launch, messages and reports with
  `agent read <ref>:s3` or `agent outline <ref>:s3`, without `--subagents`.
  - The `continue read` record after such a read or outline lists that
    sub-agent's messages alone.
- Show a sub-agent's launch prompt under its launching call in `agent read`
  with tools.
- Add `similarity=` and `word-match=` to `agent search` and `agent within`
  hits with semantic evidence, so an agent can tell a strong match from the
  best of a weak set. Ported from claude-history v0.1.75.
- Shorten `agent read` output about 1.2–1.4x for tool-heavy ranges (on one
  example corpus); a read of mostly prose gains little, about 1.1x.
  - Each record opens with its address and time, such as `m1021 01:35:31`,
    without `message`, `role=` and `line=`; `anchor=` shows only on a read of
    one message.
  - A message names its source only when that is not the agent: `user`,
    `report` for a sub-agent's or a background task's report, or `result`
    for a tool result that does not name its call.
  - A message repeating one shown whole earlier shows `same=m1007` in place
    of a longer text, and a call `same=m1006.1`. A message or call repeating
    another sub-agent's, or the session's, prints its own lines.
- Shorten `agent outline` message records to the read's shape, such as
  `m12 user 14:30:12 chars=340 | …`, without `role=` and `anchor=`; a tool
  result names its call and its tool, such as `call=m1006.15 tool=PowerShell`.
  Segments no longer show `anchors=`.
- Separate a session title from its record in `agent search`, `agent within`
  and `agent sessions` with a lone colon, such as `total=1 : fix auth cache`;
  ` | ` now marks message content alone.

### Fixes

#### All agents

- Fix `--local` and the workspace filter missing the repository's sessions
  from a subdirectory or a linked worktree. They now show the sessions
  recorded anywhere in the repository: its main checkout, every worktree and
  their subdirectories.
  - A session from a worktree you have since deleted shows:
    - while `git worktree list` lists the worktree
    - when the worktree was inside the main checkout, such as under
      `.worktrees/` or `.claude/worktrees/`
    - for Claude Code, when the worktree was workmux's
      `<repo>__worktrees/<branch>`
  - Sessions from other deleted worktrees no longer show under `--local`,
    only without it.
  - `agent search --local` and `delete-empty --local` cover the same
    sessions.
- Fix `agent search` and the session list choosing a session by its last
  message. A session still active after `--before` was left out, and a hit
  could be older than `--since`. Both now keep a session that has a message
  inside the window, and search hits come only from those messages.
- Fix quoted text matching across two messages, or across two parts of one
  message, in the session list and in `agent search`.
- Fix the session list going blank while a semantic search runs. Ported from
  claude-history v0.1.75.
- Fix the session list's message count disagreeing with `agent read` in
  sessions holding sub-agents or tool results without text.
  - A Claude Code sub-agent's reply now shows as one message in `agent read`,
    not one message per record.
  - A Claude Code slash command's output and error now show in `agent read`,
    each as a message.
- Fix sessions missing from an exact `agent search` for text in one of their
  sub-agents, and their token counts leaving that sub-agent out.
  - A sub-agent without user text was missing. Most Codex sub-agents have no
    user text: the parent agent sends the task as a message. Few Claude Code
    sub-agents lack it.
- Fix directory listings showing their input as JSON; show them as
  `listed N directories` in summary mode:
  - Pi's `ls`
  - OpenCode's `list` in sessions from before 1.0.201
  - Claude Code's `LS` in older sessions
- Fix summary mode counting web searches as `searched N webs`; it now shows
  `searched the web N times`.
- Fix a failed delete from the `Ctrl+X` actions menu doing nothing visible: the
  status bar now shows why.
- Fix `delete-empty` listing a session as never answered when the agent
  replied to an opening tool result before the first prompt.
- Fix the semantic model downloading again after you clear the semantic
  cache or move rearview's cache. The model now stays in the Hugging Face
  cache, which the `hf` CLI also uses.
  - The model is downloaded to the directory specified by
    `FASTEMBED_CACHE_DIR`, or absent that, to the
    [Hugging Face cache location](https://huggingface.co/docs/hub/main/en/local-cache#cache-location)
    (default `~/.cache/huggingface/hub`).
  - After upgrading, the first semantic search moves the model from
    rearview's cache into the Hugging Face cache, without a download.
- Fix the semantic model downloading into `HF_HOME` itself, outside its `hub`
  folder, when `HF_HOME` is set. It now downloads into that `hub` folder on
  the next semantic search; delete the `models--Xenova--bge-small-en-v1.5`
  folder left in `HF_HOME` to free its space.

#### `rearview agent` commands

- Fix message times missing from `agent read`, `agent outline`, `agent search`
  and `agent within`. Each message now shows the local time it was written,
  such as `11:41:28` below a `day 2026-10-14` record, and each hit shows
  `at=2026-10-14T11:41:28`. The header's `tz=` names the time zone offset.
  - A message written over several seconds, such as a Claude Code reply with
    tool calls, shows its first and last time, such as `11:41:28..11:41:30`;
    a hit shows `to=`.
  - An outline segment shows the first and last time of its messages.
  - After a daylight-saving change, a `day` record shows the new time zone
    offset, such as `day 2026-11-02 tz=-05:00`, and a hit's, segment's or
    session's time shows its own, such as `at=2026-11-02T10:00:00-05:00`. `--since`,
    `--after` and `--before` accept such a time.
  - A message whose times cross the change shows its last time in full with
    the new time zone offset, such as `01:50:00..2026-11-01T01:10:00-05:00`.
  - The times count toward `--budget`: about 4–7% of a tool-heavy read (on
    one example corpus).
- Fix the command a tool call ran missing from `agent read` with tools,
  `agent search` and `agent within`. Each call now shows as one line naming
  its tool and its command, file, pattern or URL, such as `Bash: cargo test`.
  `agent search` and `agent within` find that line.
  - A command of several lines shows its first non-empty line and counts the
    others, such as `Bash: cd src (+4 lines)`. `agent search` and
    `agent within` find every line, and a read of one message, such as
    `agent read <ref>:m7 --tools`, shows every line.
- Fix `agent read` not showing which tool call a result answers. With
  `--tool-results`, a result now shows its call's address, such as
  `call=m1006.15`, with or without `--tools`. With `--tools`, each call shows
  as a record of its own with its time and the message holding its result,
  such as `m1006.15 01:35:07 result=m1021`.
  - `agent read <ref>:m1006.15` shows that call with every line of its
    command, then its result, without `--tools`.
  - A result whose call the read does not print also names the call's tool,
    such as `tool=PowerShell`.
  - A run of hidden results names the calls they answer that the read does
    not print, such as `calls=m1006.1..15`.
  - A result in its call's own message, such as the output of a shell command
    you ran, shows as its own record after the call, such as
    `m5.1 result 10:00:03`.
  - With `--lines` or `--match`, a call's numbered lines show below its record.
- Fix `agent read` and `agent outline` not naming which sub-agent wrote a
  message. Each sub-agent's records now carry its handle, such as `s3`, and a
  record such as `subagent s3 type=Explore launch=m255 span=m258..m410
  report=m411 : Review the parser` names it.
  - Its launching call shows `launches=s3` and a call sending to it
    `target=s3`, as does a run of hidden messages holding such a call; its
    report shows `from=s3`.
  - A Claude Code `Agent` call run in the foreground returns the sub-agent's
    final reply; that result now shows `from=s3`, and `report=` names it.
  - `report=` names only messages carrying the report; a notice that the
    sub-agent finished shows `from=s3` alone.
  - An outline segment names the sub-agents whose messages it holds, such as
    `seg m1..m20 s3,s4`.
- Fix a read of one sub-agent's message or call, such as
  `agent read <ref>:m7`, showing it as hidden without `--subagents`. It now
  shows the message or call whoever wrote it.
- Fix `agent outline` rejecting a range after its ref, such as
  `<ref>:m7..m9`. It now outlines that range.
- Fix a ref with an unknown part after its conversation, such as
  `<ref>:REF:s3`, reporting that a ref must contain hexadecimal digits. It now
  names the forms the command accepts.
  - A transcript path that is not found and holds a `:` after its drive,
    such as `C:\s\x.jsonl:bad/s3`, names them too.
- Fix a message `agent read` truncated naming character offsets that
  `--lines` cannot read. It now ends with `m1006 omitted lines=41..230
  calls=m1006.4..15` and a `read` record whose `lines=` and flags show the
  rest.
- Fix `agent read` of messages its flags hide leaving out the flags that show
  them. It now ends with a `read` record, such as
  `read ref=ch_…:m401..m410 tools=false tool-results=false thinking=false subagents=true`.
  - In a read of several refs, each conversation whose messages are all
    hidden ends with its own.
  - Messages with gaps between them, such as a sub-agent's, show as one ref
    per run: `read refs=ch_…:m3..m5;ch_…:m9`.
- Fix a `continue read` record leaving out the flags that show its messages.
  It now ends with them, such as `continue read refs=ch_…:m3..m5;ch_…:m9
  tools=true tool-results=false thinking=false subagents=true`.
  - A truncated outline's record now names its messages in `refs=`, in place
    of `ref=`.
- Fix a hit's `read` recipe in `agent search` and `agent within` leaving out a
  flag its message needs, such as `subagents=false` for a sub-agent's message.
- Fix `agent` output dropping or truncating a warning record when the budget
  ran short. Warnings now print whole, and the other records share the rest
  of the budget.
  - A truncated outline's header counts them in `warning-records=`, in place
    of `warnings-emitted=0`.
- Fix the end of a long session missing from `agent outline`. An outline now
  spans the whole session: each segment holds enough messages to fit the
  outline in `--budget`.
  - The outline header now shows the session's message count, such as
    `messages=4366`, and its first and last time.
- Fix `agent read` not reporting the tool calls, tool results, thinking and
  sub-agent messages it hides.
  - The header now counts what its records hide, by kind, such as
    `hidden-tools=37 hidden-tool-results=36`.
  - A message names the calls it hides and counts its other hidden parts,
    such as `hidden-tools=Write,PowerShell hidden-thinking=1`.
  - Consecutive messages hidden whole show as one record with their times
    and counts, such as
    `m1007..m1019 hidden 01:33:50..01:35:03 tools=3 tool-results=12`. A run
    contains either the session's messages or one sub-agent's, and a
    sub-agent's run carries its handle.
  - The reports count toward `--budget`: they add about 2–8% to a tool-heavy
    read, or up to about 13% with `--tools` (on one example corpus).
- Fix `agent read` and `agent outline` with `--thinking` printing a bare
  `thinking:` line for each thinking part without text, as Claude Code
  records most.
- Fix `agent search` not showing the session with the best match inside the
  window. Lexical and exact `agent search` now rank each session only on its
  messages inside the window.
  - A title, project name or summary now adds to the rank only when those
    messages match.
- Fix `agent read`, `agent outline` and `agent within` not supporting a time
  window. They now show and search only the messages inside the window.
  - `agent read` with a message range shows the messages in the range and in
    the window.
  - The `agent read` and `agent outline` headers report
    `messages-in-window=N`.
- Fix semantic and hybrid `agent search` hits pointing at an earlier message
  than the matched message.
  - Each earlier sub-agent message, and each earlier tool result without text
    (a `ToolSearch` result or an empty command output), moved the hit back
    one message.
- Fix `agent read --budget` without `--focus` stopping before a message too
  long to show whole, with budget left over. It now truncates that message
  and shows the messages after it.
- Fix `agent within` and `agent search --flat` repeating a session's `title`
  record before each hit. The title now appears once, before the session's
  first hit.

#### Claude Code

- Fix Claude Code sessions from 2.1.63 on missing the slash commands you ran
  and their output.
  - A session where you only ran slash commands now appears in the list.
- Fix Claude Code sessions missing from a search for one of their slash
  commands with its arguments, such as `/model opus`.
- Fix a prompt quoting a `<command-name>` tag showing as only that slash
  command in the session list and `agent read`.
- Fix Claude Code sessions from 2.1.97 on missing the prompts you typed while
  Claude was working.
  - Background agent, background command and sub-agent reports that arrived
    while Claude was working now show as `Task` rows.
- Fix Claude Code sessions from 2.1.97 on missing the files and directories
  you attached with `@`, and the files Claude Code attached again after a
  compaction.
  - They now show as reads and directory listings under `You`.
  - A file too large to attach again now shows as a reference in the same
    run: `Read 2 files, referenced 3 files`.
- Fix Claude Code sessions from 2.1.97 on missing hook messages and hook
  errors.
  - Each now shows in its tool call's result; a hook with no tool call shows as
    a `Hook` row.
- Fix Claude Code sessions from 2.1.97 on missing IDE diagnostics.
  - They now show as an `Issues` row: Claude Code's count line, then each
    file's issues, truncated.
- Fix Claude Code sessions from 2.1.108 on missing session recaps.
  - They now show as `Recap` rows behind the thinking toggle.
- Fix Claude Code sessions from 2.1.229 on missing safeguard refusal warnings.
  - They now show as `Notice` rows, as do Claude Code's backgrounding notices.
  - Claude Code's note to the model after a warning no longer shows under
    `You` as if you had typed it.
- Fix a Claude Code session's token count including each fork's launching
  `Agent` call twice (1.6% too high in one example session).
- Fix `--show-dir` printing a Claude Code sessions directory without your
  sessions when run from a subdirectory or a linked worktree. It now prints the nearest one
  holding your sessions: a parent's up to the repository's top level, or the
  main checkout's.

#### Codex

- Fix a Codex session missing every turn before a prompt you edited.
  - Editing a prompt and leaving it unsent no longer drops the session off the
    list.
  - `Ctrl+L` in the list reports how many sessions are missing earlier turns
    because an earlier session file is compressed, not found or unreadable.
- Fix a forked Codex session missing the turns from before the fork.
- Fix a Codex session showing a discarded turn's sub-agents after an edited
  prompt.
  - Each opens by its session ID.
- Fix Codex sessions missing the `Conversation interrupted` and `Goal budget
  reached` notices.
- Fix the model's web searches and opened pages missing from Codex sessions.
- Fix `/goal` goals and their status changes missing from Codex sessions.
- Fix a Codex sub-agent's report showing twice, in its own messages and where
  it handed it back. Its own copy now shows as `final_answer: report delivered`.
  - Search now finds the report where it was handed back, as `agent read`
    shows it.
- Fix Codex `send_input` calls showing their input as JSON instead of the
  sub-agent they message, and counting as `called N tools` in summary mode.
- Fix deleting a Codex session leaving its forks unable to open in Codex: the
  delete now refuses and names them.
- Fix deleting a Codex session leaving its archived files in Codex's
  `archived_sessions` folder.

#### OMP

- Fix `/clear` missing from OMP sessions; a `Context cleared` row now marks
  where you ran it.
- Fix OMP sub-agent threads missing from `rearview <file>` and `--render`.

#### Pi

- Fix Pi `powershell` calls showing their input as JSON instead of the
  command, and counting as `called N tools` in summary mode.
- Fix a Pi session's token count missing prompt-cache warming tokens from long
  tool runs.

#### Kimi Code

- Fix Kimi Code sessions showing system text under `You` as if you had typed
  it.
  - Background task notices now show as `Task` rows, hook output as `Hook`
    rows and scheduled reminders as `Scheduled` rows.
  - A blocking hook's output now shows as a `Hook` row.
  - Your `!` shell commands now show as shell commands with their output.
  - Skills and plugin commands run with `/` now show as their slash command.
  - When the model loads a skill, the skill file no longer shows.
  - Sub-agent prompts, goal continuations and a prompt's bundled skill files
    no longer show.
- Fix the `Ctrl+X` actions menu offering Fork for Kimi Code sessions:
  choosing Fork closed the list with an error. Fork is now greyed out there.

#### OpenCode

- Fix `agent search` failing with "no such table: session" on a fresh
  OpenCode 2 install.
- Fix the `Ctrl+X` actions menu offering Fork for OpenCode sessions with
  OpenCode 2 installed: choosing Fork closed the list with an error. Fork is
  now greyed out there.

### Changes

- `--show-dir` (`-d`) now takes an agent name: run `--show-dir claude` for
  Claude Code's sessions directory.
- The first launch after upgrading rebuilds every agent's session cache once.
- The delete report now counts a Codex or Claude Code session's files as
  `session files`, not `stored copies`.

## v0.5.0 (2026-10-11)

### Enhancements

- Add `Ctrl+R` to refresh without restarting: in the list, new sessions
  appear, changed ones update and deleted ones are removed; in an open
  session, new messages appear.
- Speed up `agent search` and search in the session list (on one example
  corpus, on a 4-core machine):
  - ~11x for exact `agent search`, though little for rare words
  - ~2.6x for lexical and ~2.3x for hybrid `agent search`
  - search in the session list:
    - ~1.5x for words
    - ~1.8x for quoted phrases
- Lower peak memory about 20% when loading cached sessions (on one example
  corpus); a corpus with many tool results over 16K characters gains less.

### Fixes

- Fix renaming a Claude Code session stored in two project folders replacing
  its row with the other folder's copy.
- Fix a session you renamed, or opened by pasting its ID, showing its first
  messages as its list preview instead of its latest ones.
- Fix message copy (`y`) copying the wrong message when messages or a title
  were added above it since you opened the session.
- Fix a sub-agent's messages staying missing after its transcript was locked
  while being read.
- Fix `agent search` and `agent read` missing:
  - all but the last part of a Claude Code reply, such as the text written
    before a tool call
  - some Kimi tool calls
- Fix `agent search` and `agent within` missing:
  - text past the first 16K characters of a long message or thinking block,
    or in the middle of a long tool result
  - an exact or quoted phrase split by a line break or different spacing
- Fix search in the session list missing text in the middle of a long tool
  result.
- Fix `agent search` missing sessions that match only in thinking or a tool
  call's name.
- Fix semantic search missing Claude Code text written just before a tool
  call.
- Fix list previews showing only the last text block of a Claude Code reply.
- Fix search missing a phrase split by terminal color codes in tool output.
- Fix `agent search` reporting nothing about matching sessions it did not
  search. It now reports how many sessions matched and how many it searched,
  and `--offset` searches the next ones.
- Fix search inside a session missing a phrase that wraps onto the next line.
- Fix a long tool result from an agent other than Claude Code missing its
  middle in the session view.

### Changes

- The first launch after upgrading rebuilds every agent's session cache once.

## v0.4.0 (2026-10-08)

### Enhancements

- Rebuild the session cache for agents other than Claude Code about 10x
  faster (on one example Codex corpus, with its files already in memory),
  such as on the first launch after an upgrade.
  - Sessions with many sub-agents gain the most; a load from an up-to-date
    cache gains little.
- Cut peak memory usage while indexing semantic search by about half/1.5 GB.
- Find Codex sessions about 3.5x faster (on one example corpus, with the
  files cached) by taking advantage of Codex's own session state database.
  Opening a Codex session by ID is faster for the same reason.
  - A session file the database does not list is no longer shown.
  - Codex versions its database. A database at a version newer than the one
    this release was developed against is still read; `--debug` warns once.
  - If Codex's session state database is busy or cannot be read, no Codex
    sessions are shown for that launch. The list and `agent search` output
    report it.
- If OpenCode's session state database is busy or cannot be read, no OpenCode
  sessions are shown for that launch. The list and `agent search` output now
  report it, as they do for Codex; until now `--debug` alone did.
- Show `searching` with a spinner in place of the match count while a search
  runs, plus the seconds elapsed once a second has passed. A keystroke
  restarts the search and the seconds. Pressing space at the end of the
  query no longer relaunches the search.
- Keep the results on screen highlighted for the query they answered while a
  new search runs; they change when its results arrive.
- Backspacing the query back to the one whose results are on screen abandons
  the running search and keeps those results.
- Dim the list and show `Opening…` in the key row while a session opens.
- Shard each agent's session cache so a launch after a change rewrites only
  the shards holding changed sessions, up to 16x less than before.
  - The first launch after upgrading migrates every other agent's session
    cache and rebuilds Claude Code's, once; that launch takes about 2.5x a
    normal one (on one example corpus).
- Drop the `lex` label from the search bar's result count in lexical mode,
  so `sem` alone marks the mode. Ported from claude-history v0.1.75.
- Show dates in the info view (`info·on` in the status bar): a label marks
  each new day, and the status bar shows the top row's date
  (`[2476/4503 Oct 03]`).
  - The header now shows the session's date range (`Sep 26 – Oct 03`), or
    its date and time for a session within one day.
- Start the first launch after an upgrade sooner; rearview no longer reads
  the outdated cache before rebuilding it.
- Add a `Ctrl+X` actions menu to resume, fork, delete or rename a session, in
  the list and the viewer.
  - `Ctrl+R`, `Ctrl+F` and `Ctrl+X` no longer resume, fork or delete
    directly, and the `keys.resume`, `keys.fork` and `keys.delete` settings
    no longer apply.
- Install on macOS (Apple silicon) or Linux (x86_64) through Homebrew:
  `brew install astral303/rearview/rearview`, and update with
  `brew upgrade rearview`.

### Fixes

- Fix deleting a Kimi session not reporting the sub-agent sessions it removed.
- Fix the info view's times keeping the dark theme's grey in a light theme.
- Fix the loading count standing still while a session with many sub-agents,
  or a Claude Code project with many sessions, loads; it now counts
  transcripts, each sub-agent's included.
- Fix `--render` printing nothing and exiting successfully for a file that is
  not a session transcript, and `rearview <file>` opening one as a Claude
  Code session; both now report `Not a recognized session transcript`.
- Fix `agent search --local` finding only Claude Code sessions; it now
  searches every agent's sessions recorded in the current workspace.
- Fix `agent.exclude_projects` excluding only Claude Code sessions, and only
  by their encoded folder name; it now matches the project name in the
  list's leftmost column.
  - If your setting names an encoded folder, such as `-Users-you-code-repo`,
    change it to the project name (`repo`).
- Fix `agent search` never indexing new sessions: a semantic or hybrid search
  now catches up the semantic index for up to 2 seconds before it answers,
  so the same `partial-index` warning no longer appears on every run.
  - `semantic.interactive_embedding_seconds` sets the seconds; `0` turns
    catch-up off.
- Fix every launch rewriting each agent's whole session cache even when no
  session changed.
- Fix interrupted cache writes leaving temp files on disk; rearview now
  removes them the next time it reads or writes that cache.
- Fix `REARVIEW_CACHE_DIR` leaving semantic search's embedding cache and
  model in `~/.cache/rearview`; they now live under the directory it names.
  - If you set it, move `~/.cache/rearview/semantic` into that directory to
    keep your embedding cache; otherwise the next semantic search downloads
    the model and embeds every session again.
- Fix an unknown OpenCode or Kimi session ID running as a text search; the
  list now shows "not found", as it does for a UUID.
- Fix a session ID pasted in uppercase reported as not found even though the
  session existed. A UUID now matches in any case; an OpenCode ID matches as
  OpenCode wrote it.
- Fix a Claude session reopened without a new message jumping to the top of
  the list and into the `--since` window; it is now dated by its last message.
- Fix `→`, `←` and `Enter` not expanding or collapsing truncated messages in
  `tools·trn`.
- Fix `→`, `←` and `Enter` on a call in `tools·sum` expanding or collapsing
  its truncated input and result one at a time; they now act on both together.
- Output with one line truncated now simply shows that line instead of
  `(1 more lines...)`.
- Fix blank lines at either end of a tool result showing and counting toward
  its truncation.
- Fix a sub-agent's tool results in `tools·trn` and `tools·all` showing as
  escaped JSON under a `↳ Tool │ <Result>` row, with a literal `\n` for each
  line break. They now show as text beside `Result`.
- Fix `tools·sum` showing one unexpandable row per sub-agent tool call. A
  sub-agent's consecutive calls now collapse into one row that expands.
- Fix a skill's text showing after the `Skill` call or slash command that
  loaded it.
- Fix a call's result missing from its expanded `tools·sum` row when another
  agent's row came between the call and its result.
- Fix skill loads in Claude Code, Kimi Code and OpenCode sessions counted
  under `called N tools` in `tools·sum`; they now show as `loaded N skills`.
  - A skill call in `tools·trn` and `tools·all` now names the skill in its
    header, with any arguments below.
- Fix a long sub-agent reply showing whole in `tools·sum` and `tools·trn`; it
  is now truncated to four lines.
- Fix sub-agent threads missing from exports when thinking is shown.
- Fix exports and copied messages leaving out all but the first text block of
  a message with several, such as your prompt after an OpenCode review
  comment.
- Fix Plain and Markdown exports printing an empty `Thinking` row for each
  empty thinking block, and copied messages an empty paragraph.
- Fix the `Ctrl+O` printout showing nothing for agents other than Claude
  Code.
- Fix the `Ctrl+O` printout showing tool results under `<Result>` rows, with
  sub-agent results as JSON; it now prints the viewer's rows.
  - With tools hidden (`--no-tools`, the default), its ledger form now shows
    the viewer's summary rows, such as `Ran 1 shell command`, where it showed
    no tool rows.
- Fix concurrent Codex sub-agents showing the same `↳` label; each
  sub-agent's rows now carry its Codex nickname (`↳Lorentz`).
- Fix Codex agent calls showing their message as encrypted text. A
  `spawn_agent` call now shows the sub-agent's type, model, effort and
  forked turns, and its result names the sub-agent it started
  (`Lorentz (suite_runner) · /root/scout`).
- Fix a Codex sub-agent's final answer showing as `called 1 tool` with its
  text in JSON; it now shows as a `Task` row with the report.
- Fix exports printing a `<local-command-caveat>` notice and each `/clear`
  under `You`.
- Fix Ledger exports printing the `*` and `**` Markdown marks in your
  messages.
- Fix a Claude Code background agent's result showing `Async agent launched
  successfully…` and instructions meant for the model; it now reads `Running
  in the background`.
- Fix a finished Claude Code background agent or background command showing
  under `You` as rows of `<task-notification>` machine state. It now shows as
  a `Task` row: the summary, then for an agent its tokens, tool calls and
  duration, then the report.
  - The viewer truncates the report to four lines in `tools·sum` and
    `tools·trn`; `→`, `Enter` or a click expands it.
  - Printed whole in `--render`, the exports and the agent CLI.
  - The list preview shows the summary, and search indexes the summary and
    the report; a session no longer matches on the notification's own words
    (`task-notification`, `output-file`).
- Fix a Claude Code sub-agent's handed-back report showing under `You` inside
  text meant for the model; it now shows as a `Task` row, and the later
  `finished` row shows only its summary and usage.
- Fix Claude sub-agent threads missing from the viewer, exports, the
  `Ctrl+O` printout, `--render` and `agent search`. A sub-agent the session
  ran through the `Agent` tool now shows nested under its agent type
  (`Explore`, `general-purpose`) behind the thinking toggle, as for Codex and
  Kimi; `agent search` finds its text through the session; the session's row
  counts its messages and tokens.
  - A nested sub-agent, one a sub-agent ran, shows under the same session.
  - Deleting a Claude session now reports the sub-agent transcripts deleted
    with it.
  - The first launch after upgrading rebuilds the Claude session cache once.
- Fix Codex Guardian ("approve for me") reviews counting toward a session's
  messages and tokens and appearing in its `agent search` text. Codex runs a
  Guardian review as a thread of its own that restates the session it
  reviewed; it is now neither listed nor read.
  - A Guardian review's ID no longer opens a session.
  - The first run after upgrading rebuilds every provider's session cache once.
- Fix OMP sub-agent transcripts, the `.jsonl` files in a session's artifacts
  directory, missing from the viewer, exports and `agent search`.
  - Each now shows nested under its output id behind the thinking toggle, as
    for Codex and Kimi.
  - `agent search` finds its text through the session.
  - The session's row counts its messages and tokens.
  - Deleting the session reports them.
- Fix a Codex, Kimi or OpenCode session opened by pasting its ID showing fewer
  messages and tokens than its list row.
- Fix a renamed session's row losing its sub-agent messages, tokens and search
  text until the next launch.
- Fix semantic and hybrid `agent search` not finding a session by text that
  appears only in one of its sub-agent transcripts.
- Fix a Pi or OMP tool result row showing no timestamp with `i` on, and a run's
  duration ending at its last call. A session ending in a tool result is now
  dated from that result.
- Fix `--delete <SESSION_ID>` refusing a Pi or OMP session as ambiguous when
  `PI_CODING_AGENT_SESSION_DIR` points both agents at one directory.
- Fix Pi and OMP sessions missing when `PI_CODING_AGENT_SESSION_DIR` or
  `PI_CODING_AGENT_DIR` is set but empty; an empty variable now counts as
  unset.
  - OMP sessions were also missing when `PI_CONFIG_DIR` was set but empty.
- Fix `agent search` run from inside a Claude Code or Codex session returning
  that session and counting its passages as missing from the semantic index;
  `--include-current-session` keeps it.
- Fix passages missing from the semantic index reaching `agent search` and
  `agent within` callers as a plain stderr line. It is now a `partial-index`
  warning record giving `<missing> of <total> passages` and the remedy.
- Fix the `sem embedding N%` progress disappearing from the bottom bar while
  you type a new semantic query. Ported from claude-history v0.1.75.
- Fix `agent search` and `agent within` labeling a lexical hit `exact` when
  its preview is a quoted sentence. Ported from claude-history v0.1.76.
- Fix a click in the list during a quoted search selecting the wrong session
  when a row above already showed the quoted text. Ported from claude-history
  v0.1.76.

### Internal: Codex session discovery

- Sessions and their sub-agents are now listed from the database's `threads`
  and `thread_spawn_edges` tables.
- Session files are read only when the database file is absent.

## v0.3.1 (2026-09-02)

### Fixes

- Fix a shell command you ran in Pi missing altogether in the default tools
  summary mode, and showing as a result with no call in the other two. It now
  reads as your own call, `You ┤ ran <your command>`, with its output as the
  result below it, and collapses to `You │ Ran 1 shell command` in a run of its
  own, separate from the agent's.
  - The ledger, plain and Markdown exports and `Copy to clipboard` carry the
    command too.
  - The command now counts toward the session's date and duration.
  - The result of a command that failed now ends `Exit code: 1` rather than
    `Exited with code 1`.
  - The TUI's search no longer finds the command, as it never found the
    agent's own shell commands. `agent search` still finds it.
- Fix a shell command you ran in Codex not showing at all. It now reads as your
  own call, `You ┤ ran <your command>`, with its output as the result below it.
  - The result of a command that failed ends `Exit code: 1`.
  - Terminal styling is stripped, so a coloured error reads as plain text.
  - The command now counts toward the session's date and duration.
- Fix a tool result injected into a Codex session showing nowhere in the
  default tools summary mode, and as an unnamed `Result` in the other two. The
  `Result` row now names the tool the result carries, and the run collapses to
  `Received 1 tool result` under the session's agent. Mixed with calls it reads
  `Ran 2 shell commands, received 1 tool result`.
  - A client can inject an output under a tool's authority, such as another
    agent delegating a task, with no call from the model behind it.
  - The ledger, plain and Markdown exports name the tool too, where they showed
    the result unnamed.
- Fix archived OpenCode sessions not listing. An archived session now lists,
  searches, and resolves by session ID like any other.

## v0.3.0 (2026-08-28)

### Enhancements

- Join each tool call to its result with a thin connector, in truncated and
  full mode and inside an expanded run, so a result that arrived after another
  call's still reads as that call's. Interleaved calls run their connectors
  side by side, each in a colour of its own that its rule and tool word share;
  `Result` drops its `↳`.
  - A call issued while an earlier call still awaits its result takes the
    next lane to the right, so connectors run beside each other instead of
    crossing.
  - In truncated and full mode, consecutive calls of one entry, and
    consecutive results of one entry, are separated by a blank row.
  - With a call focused inside an expanded run, the thin gutter line between
    its input and its result takes the focus colour, so a result below the
    screen still shows as part of the focus.
- Highlight a session ID in the query (in gold) once the list recognizes it.
  The `No session with ID … found` message highlights the ID the same way.
- Report Codex sessions that were ignored because Codex compressed them.
  `Ctrl+L` in the list and an `ignored` warning in `agent search` output report
  how many, so a Codex history that stops a week back is explained. Compression
  is an experimental Codex feature, off by default, that rewrites sessions older
  than a week; `rearview` does not support compressed sessions yet.
- Find empty sessions from every agent with `delete-empty`, not only Claude's. A
  session the agent never answered now counts as empty whoever recorded it, and
  each one is removed by that agent — so a Codex thread's older rollouts and a
  Kimi session's whole directory go with it.
  - The summary counts per agent, so a machine with Codex history can tell
    whose sessions are listed.
  - `--local` uses the same rule as the list's `Tab` filter, so the two agree
    on what the current workspace holds — Claude's worktrees included.
  - A transcript holding no conversation at all — Claude metadata-only or
    malformed files — is no longer listed. Emptiness is now read from the
    parsed session, and those parse into nothing.

- Improve collapsed tool rows in tools summary mode and add complete keyboard-only
  navigation, supporting expand and collapse.
  - An expanded run keeps a `Called N tools (expanded):` heading above its
    calls; click the heading to collapse the run.
  - `Enter` expands or collapses the focused message's run.
  - `→` expands a focused message's run, or expands a focused call's output within.
  - `←` shrinks the output of a focused call from full to trimmed, or focuses the 
    whole run of calls when on a trimmed call, or collapses the whole focused run 
    to one row.
  - Inside an expanded run, `[`/`]` and `J`/`K` move between calls.
  - `y` copies the focused call or whole expanded run.
  - Scrolling moves the focus call by call within expanded runs, once the
    focused call scrolls off screen.
  - Add run duration to the summary row when timing is shown (`i`), e.g. 
    `Called 9 tools · 7m`.
- Ensure loading indicator shows each provider's progress instead of looking stuck
  for large non-Claude corpuses. 
  - Conversations appear as soon as a provider finishes loading.
  - The cursor no longer flashes over the status line.
- Install and update on Windows with Scoop, which checks the download against
  the published hash and shims `rearview.exe` onto `PATH`.
  - `scoop bucket add rearview https://github.com/astral303/scoop-rearview`,
    then `scoop install rearview`.
  - `rearview update` declines on a Scoop-managed install and points at
    `scoop update rearview`, so the two cannot disagree about the version.
  - The manual zip is still there, and the README now says how to clear the
    download mark Windows puts on it.
- Indicate when the conversation list is filtered. `^L filters` appears beside
  the result count; press `Ctrl+L` to list the active filters.
- Ship the macOS program as a single file, with ONNX Runtime built into it
  instead of bundled beside it as a separate library.
  - The README now says how to clear the download mark macOS puts on a
    manually downloaded archive.

### Fixes

- Fix slash-command output (such as `/compact` or `/add-dir`) showing stray
  `[2m` and `[22m` around its text in the viewer and in exports.
- Fix a scroll moving the focus off a message that was still on screen. The
  focus now moves only when the focused message scrolls off screen: to the
  first message on screen when it scrolled off the top, and to the last when
  it scrolled off the bottom. A focused call inside an expanded run follows
  the same rule.
- Fix a tool call's long lines being cut off at the viewer's right edge. An
  edit's diff, an agent's prompt, a header's file path and the run's summary
  row now wrap at the content width, as replies and results already do.
  - A shell command's later rows sit under its first, with no blank row
    between; they no longer stop short by the width of `PowerShell: ` or
    break after a hyphen.
  - In truncated tool display (`t` until the status bar reads `tools·trn`),
    `(N more lines...)` now counts wrapped rows, not source lines.
- Fix rows being cut off at the right edge while timestamps are shown (`i`).
- Fix Codex sub-agent threads showing nothing once Codex's session migration
  has rewritten them. A rewritten sub-agent thread now keeps its dialogue in
  search and in the parent's view; the Codex session cache is rebuilt once.
  - The migration is Codex's `background_paginated_rollout_migration` feature
    (off by default) or `codex migrate-rollouts --apply`; it rewrites sessions
    written before paginated history.
- Fix an edit's diff showing gray inside an expanded tool run in summary mode,
  and in a sub-agent's calls. Added and removed lines now keep their green and
  red while the rest of the run stays dimmed.
- Fix semantic search hanging with no message when ONNX Runtime is missing. It
  now names the directories it searched and stops.
- Fix Codex runs showing one `Called 1 tool` row per call. A run of 
  consecutive tool calls now correctly coalesces into a `Called N tools` row 
  across empty metadata-like entries and thinking turns with thinking 
  hidden.
- Fix some Claude and all non-Claude tools showing as non-differentiated, 
  general "tools" in summaries:
  - Claude: `PowerShell`, `Agent`, task updates, messages to agents.
  - Codex: `exec` scripts, `apply_patch`, `spawn_agent`, waits, plan updates.
    An `apply_patch` counts as one edit per file it touches.
  - Kimi: `Bash`, `Read`, `Edit`, `Write`, `Grep`, `Glob`, `Agent`,
    `TodoList`, `FetchURL`, waits on `TaskOutput`.
  - Pi, OMP and OpenCode: shell commands, reads, edits, writes, searches,
    agents, task updates, fetches. 
  - A Pi `edit` shows its replacements as a coloured diff.
  - An OMP hashline `edit` counts as one edit per file.
- Correctly show tool headers and bodies for non-Claude providers:
  - Codex, Kimi, Pi, OMP and OpenCode headers show the command, file or agent
    instead of raw input.
  - Headers open with the tool's own name: `WebFetch:` and `WebSearch:`
    replace `Fetch:` and `Search:`.
  - Edit bodies use git's bare `+`/`-` signs, and only diff bodies are
    coloured: a Markdown bullet in an agent prompt no longer reads as a
    removal.
- Fix `I` copy the actual session ID the provider recorded, not the 
  transcript's file name, which was a wrong value for every provider 
  but Claude and OpenCode. A conversation with no recoverable ID says so.
- Fix a pasted session ID selecting a conversation only for Claude. An ID from
  any supported agent now selects its conversation.
  - For Pi and OMP, sessions must match the active filter. For all other agents,
    sessions outside the filter are found.
  - An ID no agent has now clears the results and reports that no session was
    found, with the ID in quotes as the way to search transcript text for it
    instead. Previously the results already on screen stayed put, which looked
    like nothing happened.
- Fix `--delete <SESSION_ID>` reporting "not found" for every session but
  Claude's. It now removes a Codex thread's older rollouts and a Kimi session's
  whole directory, and says how many stored copies went when more than one did.
  - Pi and OMP record a session's ID inside the log, where two logs in one
    project can carry the same one. Where an ID names more than one session,
    the command names them all and deletes nothing.
- Fix deleting a Codex thread or OpenCode session leaving its sub-agent sessions
  in the list. They are now also deleted and included in the count `--delete`
  prints.
- Fix `--semantic` output and the semantic details popup showing transcript
  filenames where they name a session. Existing embeddings are reused.
- Fix keys help `?` to now list the mouse and `Ctrl+C`, and say that 
  `Esc` clears the list's search before it quits.
- Fix slow startup and slow `rearview agent` commands for non-Claude 
  providers: about 2.6x faster (on one example corpus).
  - The first run after upgrading rebuilds every session cache, Claude's
    included; the non-Claude rebuild takes about 4x a normal load, once.
  - A Codex, Kimi Code, OpenCode, Pi or OMP transcript that holds no
    conversation of its own is now recorded as empty, so later loads skip it
    instead of reading and parsing it in full every time.
  - Codex writes one transcript per sub-agent thread, so a workflow that
    spawns many agents gains the most. In the example corpus 77% of
    transcripts held no conversation of their own; one where most do gains
    little.
- Fix `$(rearview --show-id)` and `$(rearview --show-path)` on macOS and Linux
  capturing terminal control sequences along with the value. Ported from
  claude-history v0.1.74
  ([raine/claude-history#60](https://github.com/raine/claude-history/issues/60)).
  - When standard output is not a terminal, the list no longer asks the
    terminal for its background and uses the dark theme.

### Internal: Install and Releases

- Windows x64 binaries ship with each release — tar.gz for
  `rearview update`, zip for manual download — and `rearview update` runs on
  Windows.
- Binaries are stripped on every platform.
- Install with the script, `cargo install rearview`, or `rearview update`.
- Switch to Mise from Nix and Nix flakes for dev environment automation.

## v0.2.0 (2026-08-21)

- Renamed to `rearview`. This is a multi-provider fork of
  [raine/claude-history](https://github.com/raine/claude-history); the binary,
  the crate, the repository (`astral303/rearview`), and the companion skill
  (`skills/rearview`) carry the new name. The config file moves to
  `~/.config/rearview/config.toml`; an existing
  `~/.config/claude-history/config.toml` is copied there on first start. The
  cache moves to `~/.cache/rearview`; move the old directory to keep the
  semantic-search cache. `CLAUDE_HISTORY_CLIPBOARD` is now `REARVIEW_CLIPBOARD`.
- `rearview update` installs this fork's GitHub releases. There is no Homebrew
  tap; install with the script, `cargo install --git`, or Nix.
- Added OpenCode support: sessions are read from OpenCode's SQLite database
  (`OPENCODE_DB` overrides the path), read-only, with list, search, view,
  export, resume, fork, rename, and delete. Child sessions fold into the session
  that spawned them. A database newer than the schema this reader was verified
  against logs a `--debug` warning.
- Added Codex support: rollout sessions list, search, view, resume, fork,
  rename, and delete alongside Claude, Pi, and OMP.
- Added Kimi Code support: the same, minus fork — the Kimi CLI cannot start a
  forked session; use `/fork` inside a resumed session.
- Sub-agent threads (Codex rollouts, Kimi agent wires) fold into the session
  that spawned them: searchable through it, visible behind the thinking
  toggle, never separate list rows.
- A reverted Codex thread lists once, as its newest rollout — the file
  `codex resume` itself would open.
- Codex reasoning is encrypted, so Codex conversations usually show no
  thinking blocks.
- Renaming a Codex or Kimi session now survives a restart: titles stored
  beside the transcript are re-read on warm loads instead of coming back stale
  from the session cache.
- Added `REARVIEW_CACHE_DIR` to relocate the session cache.
- Deleting a session whose transcript cannot be read reports the read failure
  and its path instead of "session not found".
- OMP no longer claims an untitled Pi transcript because `omp` appears in its
  path.
- Parse-error line numbers in `--debug` output no longer drift one line early
  after a message is filtered out of the preview.

## v0.1.73 (2026-08-19)

- New: Browse, search, render, export, and manage Pi and OMP sessions alongside
  Claude Code history, including resume, fork, rename, and delete
- Semantic search returns results much faster and uses far less of the
  embedding cache, and global agent search no longer embeds missing passages
  on demand
- Fix: Typing in semantic mode shows lexical matches immediately while the semantic
  results are still being computed

## v0.1.72 (2026-07-31)

- Filter conversation lists and searches by relative age or local date with
  `--since`, `--after`, and `--before`
  ([#67](https://github.com/raine/claude-history/pull/67))
- Clipboard actions in SSH and mosh sessions send copied text to the terminal
  client's clipboard.
- Long custom session titles use the available row width on wider terminals,
  making similar titles easier to distinguish.

## v0.1.71 (2026-07-21)

- Claude Code agents can search and read conversation history through a safer,
  more reliable companion skill with bounded output, durable message references,
  and clear error reporting.
- Agent searches produce better-ranked evidence, preserve useful results when
  some transcripts cannot be read, and support configurable scope, search mode,
  result depth, and content visibility.
- Large individual messages can be read by line range or searched for matching
  text with bounded context.
- Semantic searches started while the embedding cache is being generated no
  longer appear stalled after the query changes.

## v0.1.70 (2026-07-05)

- Added `delete-empty` for finding transcript files with no Claude messages,
  such as sessions that only contain slash commands
  ([#61](https://github.com/raine/claude-history/issues/61))

## v0.1.69 (2026-07-04)

- Resuming a session uses the directory that owns the selected conversation, so
  duplicate session IDs open in the expected project.

## v0.1.68 (2026-06-29)

- `--show-id` keeps the selected session ID on stdout while the TUI renders on
  stderr, making it safe to use from scripts and other tools
  ([#60](https://github.com/raine/claude-history/issues/60))
- Agent search results include each conversation UUID alongside the `ref=ch_...`
  handles, making it clearer which ID to report and which handle to pass to
  follow-up commands

## v0.1.67 (2026-06-13)

- Prebuilt macOS and Linux releases now include ONNX Runtime, so semantic
  search works after install or self-update without a separate library setup
  ([#57](https://github.com/raine/claude-history/issues/57))

## v0.1.66 (2026-06-13)

- Linux prebuilt binaries now run on more older Linux distributions instead of
  requiring GLIBC 2.38 or newer
  ([#57](https://github.com/raine/claude-history/issues/57)

## v0.1.65 (2026-06-03)

- Fixed a crash in agent search and outline commands when a conversation
  included attached documents or other unsupported content blocks
  ([#52](https://github.com/raine/claude-history/pull/52))

## v0.1.64 (2026-05-30)

- Fixed Claude Code auto-generated session titles appearing as missing or
  untitled in the conversation list and search results
  ([#51](https://github.com/raine/claude-history/issues/51))

## v0.1.63 (2026-05-30)

- Added `claude-history agent` commands so agents can search, narrow, outline,
  and read bounded excerpts from your Claude history.
- Added a companion Claude Code search skill for using the agent protocol
  without pasting whole transcripts into context.
- Added `[search].mode` for choosing the default search mode, including lexical,
  semantic, exact, and hybrid options.
- Fixed Homebrew upgrade metadata so Apple Silicon installs no longer fail while
  looking for unavailable Intel macOS bottles
  ([#50](https://github.com/raine/claude-history/issues/50))

## v0.1.62 (2026-05-24)

- Identifier searches with underscores now stay precise, so `api_key` matches
  `api_key` without also matching `api key`.

## v0.1.61 (2026-05-23)

- Added quoted search for exact text, so you can find literals like tokens,
  flags, and identifiers without fuzzy word matching
  ([#48](https://github.com/raine/claude-history/issues/48))

## v0.1.60 (2026-05-19)

- Nix installs now link ONNX Runtime from nixpkgs, so semantic search works in
  Nix builds ([#32](https://github.com/raine/claude-history/pull/32))
- Nix packaging now follows `Cargo.toml` and `Cargo.lock` automatically, making
  package updates and releases less error-prone
  ([#34](https://github.com/raine/claude-history/pull/34))
- Release checks now verify the Nix build before publishing, reducing the chance
  of broken Nix releases

## v0.1.59 (2026-05-17)

- Updated release packaging to publish only Apple Silicon macOS and glibc-based
  x86_64 Linux builds because semantic search depends on ONNX Runtime, whose
  prebuilt binaries do not support Intel macOS or musl Linux

## v0.1.58 (2026-05-17)

- Added experimental semantic search for finding conversations by meaning
  instead of only exact words

## v0.1.57 (2026-05-01)

- Tool activity now defaults to concise summaries in the TUI viewer, making long
  conversations easier to scan while still showing what kinds of actions ran
- Press `t` to cycle tool display through summary, truncated, and full details;
  clicking a summary expands that group into its truncated tool calls and results

## v0.1.56 (2026-05-01)

- Click truncated tool calls and results in the TUI viewer to expand or collapse
  just that output without changing the global tool display mode
- In-conversation search now shows the current match number and total matches
  while search is active

## v0.1.54 (2026-05-01)

- Added `tui.exclude_projects` so projects can be hidden from the TUI browse and
  search lists while remaining accessible by UUID or file path
  ([#43](https://github.com/raine/claude-history/pull/43))
- Added in-TUI session renaming from the conversation list, with a configurable
  rename keybinding
- Improved markdown rendering in the TUI, including preserved line breaks,
  indentation, tabs, and code block formatting
- Shortcut help overlays now stay usable in small terminals by clamping,
  centering, and scrolling when needed

## v0.1.53 (2026-04-17)

- Conversation viewer no longer jumps to unrelated content when toggling tool
  output (`t`), thinking (`T`), or timing (`i`), or when resizing the terminal —
  the viewport now stays anchored to the message you were reading

## v0.1.52 (2026-04-17)

- Mouse wheel scrolling in both the search result list and conversation viewer,
  and click-to-open on rows in the search result list (note: enabling mouse
  capture may interfere with click-drag text selection in some terminals — hold
  Shift, or Option on macOS, to bypass)
- Search results now show the selected position (e.g. current/total) so it's
  easier to tell where you are in the list
- Improved search snippet previews — the context line now prefers locations
  where query terms appear adjacent, instead of locking onto boilerplate matches
  that happen earlier in the conversation
- Fixed search ranking missing adjacent-phrase matches when the phrase was
  wrapped in markdown punctuation like `**media pipeline**`
- Added a Nix flake for installation on Nix systems

## v0.1.51 (2026-03-29)

- Improved search ranking — results now score matches by where they appear
  (title, project, summary, or message body), so exact project and title matches
  rank above incidental mentions in conversation text
- Search freshness scoring uses smooth decay instead of sharp cutoffs, giving
  more natural ranking between recent and older conversations

## v0.1.50 (2026-03-27)

- Pressing Esc now clears the search input first — a second Esc quits the app

## v0.1.49 (2026-03-24)

- Faster startup with per-project binary caching of parsed conversations — only
  changed files are re-parsed on subsequent launches
- Reduced memory usage by streaming JSONL lines instead of loading entire files
  into memory

## v0.1.48 (2026-03-24)

- Fixed search missing content in long conversations due to a 256K character
  truncation limit — all conversation text is now fully searchable
- Added Windows support — compilation and home directory resolution now work
  correctly on Windows ([#26](https://github.com/raine/claude-history/pull/26))

## v0.1.47 (2026-03-22)

- Fixed conversations that only contain skill invocations (e.g. `/consult`,
  `/commit`) being incorrectly filtered out as empty sessions

## v0.1.46 (2026-03-21)

- Fixed the screen freezing when holding down arrow keys or j/k to scroll — the
  view now redraws smoothly during key repeat instead of jumping when the key is
  released ([#25](https://github.com/raine/claude-history/issues/25))

## v0.1.45 (2026-03-20)

- Skill invocation prompts (e.g. from `/consult`, `/commit`) are now hidden from
  search results and shown as a concise description in the conversation viewer
  instead of displaying the full expanded prompt text

## v0.1.44 (2026-03-17)

- Added support for `CLAUDE_CONFIG_DIR` environment variable — users with custom
  Claude config directories can now use claude-history without workaround
  ([#24](https://github.com/raine/claude-history/issues/24))

## v0.1.43 (2026-03-14)

- Added `claude-history update` command for self-updating the binary directly
  from GitHub releases

## v0.1.42 (2026-03-13)

- Subagent messages are now included in J/K message navigation and single
  message copy
- Plain text mode (`--plain`) now supports pager output
- Fixed `--no-color` flag being ignored in normal (non-render) display mode
- Fixed text wrapping for CJK characters and emoji that occupy two terminal
  columns but were counted as one, causing text to overflow
- Deleting a session in the TUI (`Ctrl+X`) now removes the full session
  directory, not just the transcript file
- Fixed a potential crash when deleting a conversation while a search was
  in-flight
- Fixed conversations opened by UUID not showing project name or matching
  workspace filter
- `--fork-session` now requires `--resume` and shows an error if used alone
  instead of being silently ignored

## v0.1.41 (2026-03-13)

- Workspace filter now includes conversations from git worktrees of the same
  project, so sessions started in workmux worktrees appear alongside the main
  project's sessions
- Search result counter now shows the count relative to the current scope
  (project or global) instead of always showing the total

## v0.1.40 (2026-03-13)

- Search typing is now smoother — search runs in a background thread so
  keystrokes no longer block the UI, especially with large history
- Global view is now the default — all conversations are shown on launch instead
  of only the current workspace's sessions
  ([#21](https://github.com/raine/claude-history/pull/21))
- Added `Tab` key to toggle between global and workspace-only view in the TUI
  ([#21](https://github.com/raine/claude-history/pull/21))
- Added `-L`/`--local` flag to start with workspace filter active
- Deprecated `--global`/`-g` flag and `global` config option — global is now the
  default behavior

## v0.1.39 (2026-03-13)

- Added `--delete` flag to remove a session by its ID directly from the command
  line, e.g. `claude-history --delete <session-id>`
  ([#23](https://github.com/raine/claude-history/issues/23)
- Added `--version` flag to display the current version
  ([#22](https://github.com/raine/claude-history/issues/22))
- Invalid session IDs now show a clear error message instead of failing silently

## v0.1.38 (2026-03-13)

- Improved search for CJK (Chinese, Japanese, Korean) text — queries with CJK
  characters now match correctly even when words aren't separated by spaces
  ([#19](https://github.com/raine/claude-history/pull/19))
- Added emacs-style keybindings to the search input: `Ctrl+A`/`Ctrl+E` to jump
  to start/end, `Ctrl+B`/`Ctrl+F` to move by character, `Alt+B`/`Alt+F` and
  `Ctrl+Left`/`Ctrl+Right` to move by word, `Ctrl+K` to kill to end of line,
  `Ctrl+U` to kill to start of line
  ([#19](https://github.com/raine/claude-history/pull/19))
- Fixed cursor alignment issues with wide characters (e.g. CJK, emoji) in the
  search input and conversation viewer
  ([#19](https://github.com/raine/claude-history/pull/19))

## v0.1.37 (2026-03-13)

- Linux prebuilt binaries are now statically linked, fixing compatibility issues
  on older distros with outdated glibc versions
  ([#20](https://github.com/raine/claude-history/issues/20))

## v0.1.36 (2026-03-12)

- Added message-level navigation — press `J`/`K` or `[`/`]` to jump between
  messages in the conversation viewer, with a teal marker showing the focused
  message ([#15](https://github.com/raine/claude-history/pull/15))
- Added single message copy — press `y` to copy the currently selected message
  to the clipboard instead
  ([#15](https://github.com/raine/claude-history/pull/15))
- Fixed empty thinking blocks rendering as blank "Thinking" labels with no
  content

## v0.1.35 (2026-03-12)

- Timestamps in the conversation list now automatically switch between relative
  ("just now", "5 min ago", "2 hours ago", "yesterday") for recent sessions and
  absolute ("Mar 05, 14:30") for older ones
- Recent conversations are color-graded by recency — newest sessions appear in
  bright teal, fading to gray as they get older, making it easy to spot recent
  activity at a glance
- Removed `--relative-time`/`--absolute-time` flags and `display.relative_time`
  config option — the new hybrid format replaces both

## v0.1.34 (2026-03-12)

- Search now covers tool output (command results, file contents, grep output),
  so you can find conversations by content that previously only appeared in tool
  calls
- Search highlighting now merges adjacent matches across separators — searching
  "red team" highlights the full word `red_team` instead of just the individual
  parts
- Improved search performance in conversations with large tool outputs

## v0.1.33 (2026-03-12)

- Added automatic light/dark theme detection — the TUI now adapts its color
  scheme to match your terminal background
- Fixed arrow key navigation lag when holding keys to scroll quickly through the
  list or conversation viewer
- Fixed slow redraw when pasting text into the search field

## v0.1.32 (2026-03-12)

- Fixed clipboard copy/yank not working on Linux — now uses `wl-copy` on Wayland
  and `xclip`/`xsel` on X11, with automatic display server detection
  ([#17](https://github.com/raine/claude-history/pull/17))
- Fixed resuming sessions from deleted or ephemeral git worktrees failing with
  an error instead of gracefully recovering

## v0.1.31 (2026-03-09)

- Search now matches project names, so you can find sessions by the project they
  belong to

## v0.1.30 (2026-03-09)

- Preview panel now shows the last messages by default instead of the first, so
  you see the most recent context at a glance (use `--first` to revert)

## v0.1.29 (2026-03-09)

- Added `--fork-session` flag to resume a conversation as a fork, creating a new
  branch from an existing session
- Cross-project forking: when forking a session from a different project, the
  session files are automatically copied to the current project so Claude
  resumes in the right context
- Added configurable keybindings via the `[keys]` section in the config file,
  allowing rebinding of resume (`Ctrl+R`), fork (`Ctrl+F`), and delete
  (`Ctrl+X`) actions
- Session list search now matches session UUIDs, making it easier to find a
  specific conversation by ID
- Fixed markdown rendering issues: soft breaks no longer collapse words
  together, inline code no longer clips at block edges, and list item spacing is
  correct

## v0.1.28 (2026-03-04)

- Subagent (Task tool) messages are now nested under their parent task, keeping
  the conversation view clean and organized with `↳` prefixed entries
- Subagent internals are hidden by default and revealed with `T` or
  `--show-thinking`, same as thinking blocks
- XML-tagged content (system reminders, analysis blocks) now displays correctly
  instead of being silently stripped
- Conversations from CI or headless Claude runs that lack timestamps now parse
  and display correctly

## v0.1.27 (2026-02-26)

- Session titles (set via `/rename` in Claude Code) now appear in the
  conversation list and viewer, making it easier to find named sessions
- Search preview shows matches better now

## v0.1.26 (2026-02-18)

- Added `global = true` config option to default to global search without
  passing `-g` every time, with `--local` flag to override when needed
- Ledger export and clipboard copy now render markdown properly (headings,
  lists, code blocks, tables) and wrap long lines instead of overflowing
- Fixed high idle CPU usage (~9% down to near zero) when the TUI was sitting
  idle after loading
- Fixed search preview highlighting partial word matches instead of the actual
  search phrase
- Fixed long lines in code blocks overflowing the terminal width
- Fixed blank lines and indentation issues in ledger export

## v0.1.25 (2026-02-11)

- Added `--show-id` (`-i`) flag to print the selected conversation's session ID,
  useful for resuming with custom shell aliases (e.g.,
  `claude --resume $(claude-history --show-id)`)
- Added `I` keybinding in the viewer to copy the session ID to clipboard

## v0.1.24 (2026-02-11)

- Tool calls now default to **truncated** mode, showing the header and first few
  lines with a "(N more lines...)" indicator — a middle ground between hidden
  and full output. Press `t` to cycle through modes: off, truncated, full
- Added `--no-tools` flag to start with tools hidden (complements `--show-tools`
  for full mode)
- Tables in conversation output are now rendered with proper box-drawing borders
  instead of being collapsed into plain text

## v0.1.23 (2026-02-08)

- Fixed blank or empty message blocks occasionally appearing in conversation
  output

## v0.1.22 (2026-02-07)

- Added multi-word search support in the viewer — search for phrases like "add
  feature" to find matches containing both words
- Timestamps now display on tool calls and results in ledger view (when timing
  is enabled with `i`)
- Fixed a crash that could occur when highlighting search matches containing
  certain Unicode characters

## v0.1.21 (2026-02-05)

- Fixed timestamp alignment for subagent messages and empty messages in ledger
  view
- Fixed double blank lines appearing after tool calls with empty output
- `/clear` commands are no longer shown in conversation rendering

## v0.1.20 (2026-02-05)

- Added toggleable timing display in conversation viewer — press `i` to show
  timestamps next to each message
- Show conversation duration and model/token count in the viewer header
- Show conversation duration in the conversation list
- Added keyboard shortcuts help overlay — press `?` in any view
- Added keyboard shortcuts bar at the bottom of the conversation list
- Added `Ctrl+R` (resume) and `Ctrl+X` (delete) shortcuts to the viewer status
  bar
- Added `Ctrl+C` to quit from viewer mode
- Exports now include thinking blocks and tool calls when their display is
  toggled on
- Long bash commands in tool calls are now wrapped for readability
- Improved search highlight color for better visibility

## v0.1.19 (2026-02-04)

- Added syntax highlighting for code blocks in conversation output
- Improved tool call display with human-readable formatting instead of raw JSON
- Added Vim-style half-page navigation (Ctrl-D/Ctrl-U) in the viewer
- Added Ctrl-W to delete word before cursor in the search field
- Show conversation summary in the viewer header and search results
- Display subagent conversations in ledger view
- Added direct JSONL file input support (pass a file path as argument)
- Added `--render` flag for debugging ledger output
- Improved header layout: combined into single line when terminal width allows
- Tool/thinking toggle settings now persist within session

## v0.1.18 (2026-02-02)

- Added in-TUI conversation viewer. Press Enter to view conversations without
  leaving the TUI, with Vim-style navigation (j/k, d/u, g/G) and search (/)
- Added export and yank menus to the viewer. Press `e` to export to file or `y`
  to copy to clipboard in multiple formats (ledger, plain text, markdown, JSONL)
- Added `Y` hotkey to copy the conversation file path to clipboard
- Added `resume.default_args` config option to pass custom arguments when
  resuming conversations with `Ctrl+R`
- Improved markdown rendering: fixed spacing after numbered lists, styled
  headings with subtle color
- Fixed thinking blocks to render with italic and dimmed style
- Fixed user messages showing in wrong color in the viewer
- Improved search performance

## v0.1.17 (2026-02-01)

- Added `Ctrl+R` keybinding to resume the selected conversation directly from
  the TUI

## v0.1.16 (2026-02-01)

- Fixed a crash when using global search (`-g`) that could occur when deleting
  conversations

## v0.1.15 (2026-02-01)

- Added ability to delete conversations from the TUI (press `Ctrl+D`, confirm
  with `y`)
- Added cursor navigation in the search field with arrow keys

## v0.1.14 (2026-02-01)

- Added markdown rendering for conversation output with support for headings,
  lists, code blocks, tables, and inline formatting
- Added pager support—long conversations now open in `less` (or `$PAGER`)
- Added `--plain` flag for unformatted output
- Improved search to better match word variations (e.g., "config" now matches
  "configuration")
- Added `curl | bash` install script
- Hide caveat metadata from conversation previews

## v0.1.13 (2026-02-01)

- Replaced fzf with a built-in terminal UI

## v0.1.12 (2026-01-11)

- Fixed project path detection failing for usernames containing dots (e.g.,
  `my.user`) (Thanks @duke8585!)

## v0.1.11 (2025-12-20)

- Cleaned up fzf picker display by removing index numbers

## v0.1.10 (2025-12-15)

- Added a specific error message when fzf version is too old (requires 0.67.0+)

## v0.1.9 (2025-12-14)

- Added color highlighting to the fzf picker

## v0.1.8 (2025-12-14)

- Improved fzf UX: the timestamp stays visible when searching

## v0.1.7 (2025-12-14)

- Added `--global` (`-g`) flag to search conversations across all projects at
  once

## v0.1.6 (2025-11-29)

- Added `--all-projects` (`-a`) flag to browse conversations from any project
- Added `--show-path` (`-p`) flag to print the selected conversation's file path
- Improved fuzzy search to match against full conversation content
- Added Homebrew installation support

## v0.1.5 (2025-11-17)

- Added display of tool call inputs and results when viewing conversations
- Fixed project detection for paths containing dots or special characters

## v0.1.4 (2025-10-30)

- Added faster startup with parallel conversation loading

## v0.1.3 (2025-10-30)

- Added `--debug` flag to show diagnostic information about conversation loading
- Fixed conversations containing only `/clear` commands incorrectly appearing in
  the list
- Cleaned up `/clear` command metadata from conversation previews
- Used file modification time for more accurate conversation dates

## v0.1.2 (2025-10-29)

- Fixed display of tool results that contain structured content instead of plain
  text

## v0.1.1 (2025-10-29)

- Added configuration file support (`~/.config/claude-history/config.toml`) for
  persistent display preferences
- Added `--show-thinking` and `--hide-thinking` flags to control visibility of
  Claude's thinking blocks
- Hidden tool calls by default (use `--show-tools` or `-t` to show them)
- Added `--first` flag to show first messages in preview (inverse of `--last`)
- Added `--absolute-time` flag to explicitly use timestamps (inverse of
  `--relative-time`)
- Fixed message preview order when using `--last` flag

## v0.1.0 (2025-10-29)

- Initial release
