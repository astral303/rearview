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
tag and diff steps.

## Schema version

None: rearview checks no Claude schema version.
