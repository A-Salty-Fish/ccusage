# ccusage-adapter-cursor

Cursor account usage adapter for this fork. Cursor does not persist billed
token counts in local transcripts, so the adapter reads the signed-in desktop
app's access token and fetches `GetFilteredUsageEvents` from Cursor's
dashboard API.

This source is network-backed and will not be contributed upstream: ccusage's
policy is local files only.

## Owns

- `paths.rs` — token discovery (`CCUSAGE_CURSOR_TOKEN`, `state.vscdb`) and cache path
- `parser.rs` — dashboard events → `LoadedEntry`
- `cache.rs` — seal events older than two days; refetch only the fresh window
- `client.rs` — HTTPS pagination
- `loader.rs` — merge cache + API, `has_data`
- `report.rs` — daily / monthly / session summary shapes

## Data source

```text
POST https://api2.cursor.sh/aiserver.v1.DashboardService/GetFilteredUsageEvents
Authorization: Bearer <access token>
```

Token resolution (highest first):

1. `CCUSAGE_CURSOR_TOKEN`
2. Agent CLI `auth.json` (`%APPDATA%/Cursor/auth.json` on Windows, `~/.config/cursor/auth.json` on Linux)
3. Leftover desktop `cursorAuth/accessToken` in `state.vscdb`

Events older than two days are stored under
`${XDG_CACHE_HOME:-~/.cache}/ccusage/cursor/events-v1.json` (override with
`CCUSAGE_CURSOR_CACHE`). Later runs refetch only the unsealed window.

## Token mapping

| Cursor field                    | ccusage field                 |
| ------------------------------- | ----------------------------- |
| `tokenUsage.inputTokens`        | `input_tokens`                |
| `tokenUsage.outputTokens`       | `output_tokens`               |
| `tokenUsage.cacheWriteTokens`   | `cache_creation_input_tokens` |
| `tokenUsage.cacheReadTokens`    | `cache_read_input_tokens`     |
| `tokenUsage.totalCents / 100`   | `cost_usd`                    |
| `chargedCents / 100`            | `credits`                     |
| `conversationId` or model       | `session_id`                  |

`auto` and `display` use `totalCents`. `calculate` prices from the shared table.

## Public surface

- `loader::load_entries`
- `loader::has_data`
- `report::summarize_entries`
- `run`
