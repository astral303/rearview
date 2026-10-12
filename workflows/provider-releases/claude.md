# Claude Code releases

Steps: [provider release review](../provider-releases.md).

## Listing

```sh
gh api -H "Accept: application/vnd.github.raw" repos/anthropics/claude-code/contents/CHANGELOG.md | sed -n 's/^## \([0-9][0-9.]*\)$/\1/p'
```

Claude Code's [`CHANGELOG.md`](https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md)
has one `## <version>` heading per release, newest first; the filter prints
those versions. The changelog is also the release notes.

## Source

No published source. The review reads the changelog only and skips the clone,
tag and diff steps. The dropped-records report over local sessions (step 5)
is the main check: Claude Code writes its version on every record, and the
report counts each record under its own. The report lists each
`attachment.type` and each `system` subtype as its own kind.

- Kinds a review decided against are named in the Claude reader and counted
  as `skipped`.
- When a review decides against a kind, add it to the reader's list for its
  field.
- Every other attachment type and `system` subtype is `unrecognised`, because
  rearview shows no attachment or `system` record.
- `informational` stays `unrecognised`: besides backgrounding notices, it
  carries safeguard refusal warnings, a pending finding.

## Schema version

None: rearview checks no Claude schema version.
