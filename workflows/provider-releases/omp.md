# OMP releases

Steps: [provider release review](../provider-releases.md).

## Listing

```sh
gh release list --repo can1357/oh-my-pi --exclude-pre-releases --limit 200 --json tagName --jq '.[].tagName | ltrimstr("v")'
```

OMP tags its releases `v<version>`.

## Source

Repository: [can1357/oh-my-pi](https://github.com/can1357/oh-my-pi)

```sh
git clone --no-checkout --filter=blob:none https://github.com/can1357/oh-my-pi
```

Storage paths: the code that writes session JSONL and its artifact
directories, to establish on first review.

## Schema version

None: rearview checks no OMP schema version.
