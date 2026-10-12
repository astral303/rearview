# Pi releases

Steps: [provider release review](../provider-releases.md).

## Listing

```sh
gh release list --repo earendil-works/pi --exclude-pre-releases --limit 200 --json tagName --jq '.[].tagName | ltrimstr("v")'
```

Pi tags its releases `v<version>`.

## Source

Repository: [earendil-works/pi](https://github.com/earendil-works/pi)

```sh
git clone --no-checkout --filter=blob:none https://github.com/earendil-works/pi
```

Storage paths: the code that writes session JSONL, to establish on first
review.

## Schema version

None: rearview checks no Pi schema version.
