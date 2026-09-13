# Cursor Data Source

ccusage can report Cursor account usage (IDE Agent, Tab, and `cursor-agent` CLI) through Cursor's dashboard API. Local agent transcripts do not contain billed token counts, so this source is network-backed.

This adapter lives in this fork. Upstream ccusage does not call vendor APIs.

## Focused Views

```bash
ccusage cursor daily
ccusage cursor monthly
ccusage cursor session
```

Most users can start with unified reports such as `ccusage daily`. Add the `cursor` namespace when you want to focus the same report shape on Cursor.

## Data Source

The CLI posts to Cursor's `GetFilteredUsageEvents` endpoint using a bearer token:

1. `CCUSAGE_CURSOR_TOKEN` if set
2. Agent CLI login file (`auth.json` from `agent login` / `cursor-agent`)
3. Leftover desktop `cursorAuth/accessToken` in `state.vscdb`, if present

```text
Windows CLI: %APPDATA%\Cursor\auth.json
Linux CLI:   ${XDG_CONFIG_HOME:-~/.config}/cursor/auth.json
Desktop DB:  %APPDATA%\Cursor\User\globalStorage\state.vscdb
             ~/Library/Application Support/Cursor/User/globalStorage/state.vscdb
             ~/.config/Cursor/User/globalStorage/state.vscdb
```

The token is only sent in the `Authorization` header. It is never printed or written to the cache.

Events older than two days are stored at `${XDG_CACHE_HOME:-~/.cache}/ccusage/cursor/events-v1.json` (override with `CCUSAGE_CURSOR_CACHE`). Later runs refetch only the last two days plus any uncached gap. `--offline` reads the cache and does not call the API.

## Report Views

| Focused view             | Description                                      | See also                                |
| ------------------------ | ------------------------------------------------ | --------------------------------------- |
| `ccusage cursor daily`   | Aggregate usage by date                          | [Daily Usage](/guide/daily-reports)     |
| `ccusage cursor monthly` | Aggregate usage by month                         | [Monthly Usage](/guide/monthly-reports) |
| `ccusage cursor session` | Group usage by Cursor `conversationId` or model  | [Session Usage](/guide/session-reports) |

These views support `--json`, `--compact`, `--mode`, `--since`, `--until`, `--timezone`, and `--offline`.

## What Gets Calculated

- **Token usage** - Input, output, cache write, and cache read from each dashboard event. Input is not treated as including cache.
- **Cost (`auto` / `display`)** - `tokenUsage.totalCents / 100`, Cursor's own token-cost.
- **Credits** - `chargedCents / 100`, what the plan actually deducted (often lower than token-cost for included usage).
- **Cost (`calculate`)** - Token counts × the shared pricing table. Cursor-only model ids such as `k3` may need `pricingOverrides`.
- **Session identity** - `conversationId` when the API provides it; otherwise the model name.

## Environment Variables

| Variable                 | Description                                                          |
| ------------------------ | -------------------------------------------------------------------- |
| `CCUSAGE_CURSOR_TOKEN`   | Bearer token override (skips `state.vscdb`)                          |
| `CCUSAGE_CURSOR_CACHE`   | Explicit cache file path                                             |
| `LOG_LEVEL`              | Adjust verbosity (0 silent ... 5 trace)                              |

## Configuration

```json
{
	"cursor": {
		"defaults": {
			"offline": true
		},
		"commands": {
			"daily": {
				"json": true
			}
		}
	}
}
```

## Troubleshooting

::: details No Cursor access token found
Run `agent login` (or `cursor-agent login`), or set `CCUSAGE_CURSOR_TOKEN`. On Windows the CLI stores credentials at `%APPDATA%\Cursor\auth.json`.
:::

::: details HTTP 401 / 403
The stored access token expired. Run `agent login` again, or set a fresh `CCUSAGE_CURSOR_TOKEN`.
:::

::: details `--offline` shows nothing
Run once online so the two-day seal can fill the cache, then `--offline` will reuse it.
:::
