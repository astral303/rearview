## Unreleased

- Dropped the Nix flake and the Nix CI jobs. Install with the script,
  `cargo install rearview`, or `rearview update`.
- Windows x64 binaries ship with each release, as a tar.gz for
  `rearview update` and a zip for manual downloads; `rearview update` works on
  Windows.
- Release binaries are stripped on every platform.
- The cursor no longer flashes over the status line while it updates.
- Added a loading progress indicator, so a long startup no longer looks stuck.
  Each provider's conversations appear as soon as that provider finishes.
- Summary mode collapses a Codex run of tool calls into one `Called N tools`
  row, so a long run no longer fills the screen.
- An expanded tool run keeps a `Called N tools (expanded):` heading above its
  calls; click the heading to collapse the run.
- `Enter` expands or collapses the focused message's tool run in summary mode,
  the keyboard equivalent of clicking its row.
- Inside an expanded tool run, `→`/`←` and `J`/`K` move between its calls and
  expand or collapse their output from the keyboard; the gutter marks the
  focused call and the run around it, and `y` copies the focused call.
  Scrolling through an expanded run moves the focus call by call, and
  scrolling back to a run focuses the first call on screen.
- Summary mode names what Claude's current tools did (`PowerShell`, `Agent`,
  task updates, messages to agents) instead of counting them as tools.
- Tool headers open with the tool's own name, so `WebFetch:` and `WebSearch:`
  replace `Fetch:` and `Search:`.
- Edit bodies use git's bare `+`/`-` signs, and only diff bodies are coloured;
  a markdown bullet inside an agent prompt no longer shows as a removal.
- Summary mode names what Codex's tool calls did (`exec` scripts,
  `apply_patch`, `spawn_agent`, waits, plan updates) instead of counting them
  as tools, and their headers show the command, file or agent instead of raw
  input. An `apply_patch` touching several files counts as one edit per file.
- Summary mode names what Kimi's tool calls did (`Bash`, `Read`, `Edit`,
  `Write`, `Grep`, `Glob`, `Agent`, `TodoList`, `FetchURL`, waits on
  `TaskOutput`) instead of counting them as tools, and file headers show the
  path instead of raw input.
- Summary mode names what Pi, OMP and OpenCode tool calls did (shell
  commands, reads, edits, writes, searches, agents, task updates, fetches)
  instead of counting them as tools, and file headers show the path instead
  of raw input. A Pi `edit` shows its replacements as a coloured diff, and an
  OMP hashline `edit` counts as one edit per file it touches.
- `I` in the viewer copies the session ID the provider recorded rather than the
  transcript's file name. Only Claude and OpenCode name a transcript after its
  session, so every other provider gave a wrong value. A conversation whose ID
  cannot be determined now reports that instead of copying its file name.

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
