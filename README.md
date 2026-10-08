# gray-claude-mem

Auto-captured session memory for gray — the delta that
[gray-memory](../gray-memory) deliberately doesn't cover. gray-memory is
*curated* memory (`set`/`list`/`edit` + a frozen context snapshot);
claude-mem **auto-captures**: every turn writes one compact entry to
`~/.gray/claude-mem/<project-hash>.jsonl` and recent entries come back as
prompt context.

## How it captures

- `input/submit` records the session's last user ask (always returns `{}` —
  never rewrites or swallows input; fail open).
- `event/notify` `post_tool` accumulates tool-name counts and touched file
  paths per session.
- `event/notify` `turn_end` — if the session saw activity, writes one entry
  (~200 chars): `{"ts","cwd","m":"asked: … | did: edit×2 · store.rs"}`.
  Extraction is heuristic, no LLM in the hot path. If the `host.turn`
  capability is granted, a `host/run` turn rewrites the entry into a
  one-line "want → did" prose summary; any failure falls back to the
  heuristic text.

## Recall surfaces

- **`prompt/context`** — injects the last 5 entries for the session's cwd.
- **`mem_search {query, all?}`** — substring + all-tokens scan over the
  project's jsonl (or every project with `all`).
- **`/mem`** — `list` (last 10), `search <q>`, `clear`.

## Wire methods used

`plugin/manifest`, `input/submit`, `prompt/context`, `tool/call`,
`command/run`, `plugin/shutdown`, `event/notify` (pre_tool/post_tool/
turn_end), sidecar→host `host/run` (capability `host.turn`, optional).
Protocol 2.0.

```sh
gray plugin capabilities claude-mem --all   # optional: prose summaries
```

Works fine ungranted — `host/run` errors degrade to heuristic entries.

## Upstream

[thedotmack/claude-mem](https://github.com/thedotmack/claude-mem) —
Apache-2.0. Behavioral port of the auto-capture + recall contract; store
format and heuristics reimplemented (upstream uses a worker + sqlite +
LLM compression; this port is a single sidecar with jsonl + optional
host/run compression).

## Install

```sh
cargo install --path .
gray plugin install ~/.cargo/bin/gray-claude-mem
```
