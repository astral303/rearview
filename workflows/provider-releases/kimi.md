# Kimi Code releases

Steps: [provider release review](../provider-releases.md).

## Listing

```sh
gh release list --repo MoonshotAI/kimi-code --exclude-pre-releases --limit 200 --json tagName --jq '.[].tagName | select(startswith("@moonshot-ai/kimi-code@")) | ltrimstr("@moonshot-ai/kimi-code@")'
```

Kimi Code tags its releases `@moonshot-ai/kimi-code@<version>`. The filter
drops tags for any other package in the repository.

## Source

Repository: [MoonshotAI/kimi-code](https://github.com/MoonshotAI/kimi-code)

```sh
git clone --no-checkout --filter=blob:none https://github.com/MoonshotAI/kimi-code
```

Storage paths: the code that writes `wire.jsonl`, `state.json` and
`session_index.jsonl`, to establish on first review.

## Schema version

None: rearview checks no Kimi schema version.
