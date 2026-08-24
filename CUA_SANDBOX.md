# CuaBot sandbox verification

CuaBot provides an isolated ARM64 Ubuntu desktop for testing claude-history
without using host transcripts, caches, or executables. The project helper
builds the current working tree in Linux, deploys it into a named sandbox,
installs representative history, and launches the TUI in Kitty.

## Requirements

The shared `cua-sandbox` command owns host provisioning and session lifecycle.
It requires Docker through OrbStack or another Docker daemon and Xpra at
`/Applications/Xpra.app`.

Provision the host once:

```sh
just cua-sandbox-setup
```

The project build uses `rust:1.90-bookworm`, the `release-dynamic-ort` feature,
and persistent Docker volumes for the Cargo registry and Git dependency caches.
The matching ARM64 ONNX Runtime is cached under the host XDG cache directory and
deployed beside the executable. Set `CLAUDE_HISTORY_CUA_BUILD_IMAGE` to override
the build image.

## Start a session

Choose a unique descriptive name and inspect active sessions before starting:

```sh
scripts/cua-sandbox list
just cua-sandbox-start claude-history-show-id-a1
```

`start` runs in the foreground. Keep it running in a long-lived shell or start
it as a harness-managed background process. Wait for the server to report that
it is ready before deployment:

```sh
scripts/cua-sandbox status claude-history-show-id-a1
```

An active session belongs to its creator. Use another name unless the creator
or user explicitly hands it off.

## Build, deploy, and seed fixtures

Prepare the running session from another shell:

```sh
just cua-sandbox-prepare claude-history-show-id-a1
```

Preparation performs these operations:

1. Mounts the current working tree read-only into an ARM64 Rust container.
2. Builds `claude-history` with the packaged dynamic ONNX Runtime feature.
3. Copies the Linux binary and ARM64 ONNX Runtime libraries to
   `/home/user/claude-history-run` in the sandbox.
4. Replaces the sandbox's disposable Claude configuration and cache.
5. Installs one representative conversation with session ID
   `11111111-2222-4333-8444-555555555555`.

The build includes tracked and uncommitted working-tree changes. Host Cargo
target files and Claude data are untouched.

The individual operations are also available:

```sh
scripts/cua-sandbox build-deploy claude-history-show-id-a1
scripts/cua-sandbox fixtures claude-history-show-id-a1
```

## Launch and drive the TUI

Launch the normal picker:

```sh
just cua-sandbox-launch claude-history-show-id-a1
```

Pass CLI arguments after the session name:

```sh
just cua-sandbox-launch claude-history-show-id-a1 --show-id
```

Drive it through the shared CuaBot client:

```sh
scripts/cua-sandbox cua claude-history-show-id-a1 --key ArrowDown
scripts/cua-sandbox cua claude-history-show-id-a1 --key Enter
```

CuaBot key names follow Playwright naming, including `Enter` and `Escape`.
Allow asynchronous states to settle before sending the next input. The launched
application also runs inside a disposable tmux session. Use the project key
helper for control combinations that the Xpra browser transport does not
preserve:

```sh
just cua-sandbox-key claude-history-show-id-a1 C-o
```

## Verify `--show-id`

Launch the command-substitution check:

```sh
just cua-sandbox-verify-show-id claude-history-show-id-a1
```

The terminal runs this equivalent command with isolated configuration and cache
paths:

```sh
selected_id="$(claude-history --show-id)"
```

Select the fixture conversation with `Ctrl+O`, or send the equivalent tmux key
from the host:

```sh
just cua-sandbox-key claude-history-show-id-a1 C-o
```

The result screen must show:

```text
exit status: 0
stdout: <11111111-2222-4333-8444-555555555555>
expected: <11111111-2222-4333-8444-555555555555>
```

Any terminal query response or escape sequence inside the `stdout` brackets is
a failure.

## Screenshots

Create one task-specific host directory and retain every screenshot there:

```sh
session=claude-history-show-id-a1
screenshot_dir="/tmp/cua-sandbox-$session-screenshots"
mkdir -p "$screenshot_dir"

scripts/cua-sandbox cua "$session" \
  --screenshot "$screenshot_dir/01-picker.jpg"
scripts/cua-sandbox key "$session" C-o
scripts/cua-sandbox cua "$session" \
  --screenshot "$screenshot_dir/02-clean-stdout.jpg"
```

Inspect screenshots directly for layout, clipping, colors, focus, and exact
visible output.

## Diagnostics

Get the container name and inspect the deployed files:

```sh
container="$(scripts/cua-sandbox container claude-history-show-id-a1)"
docker exec "$container" uname -m
docker exec -u user "$container" \
  /home/user/claude-history-run/claude-history --version
```

The sandbox should report `aarch64`. Test binaries, fixtures, and caches are
disposable with the named container.

## Cleanup

Stop every session created for the task:

```sh
just cua-sandbox-stop claude-history-show-id-a1
scripts/cua-sandbox list
```

The final listing should not contain the session. The shared image, host CLI,
Playwright browser, Xpra installation, and Cargo cache volumes remain available
for later runs.
