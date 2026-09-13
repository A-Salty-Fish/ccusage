use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

use super::parser::CursorUsageEvent;

/// Events older than this are treated as immutable and served from disk.
pub(crate) const SEAL_AGE_MS: i64 = 2 * 24 * 60 * 60 * 1000;
/// First-run lookback when `--since` is omitted (matches other Cursor clients).
pub(crate) const DEFAULT_LOOKBACK_MS: i64 = 400 * 24 * 60 * 60 * 1000;

const CACHE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimeRange {
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CursorCache {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub sealed_from_ms: Option<i64>,
    #[serde(default)]
    pub sealed_to_ms: Option<i64>,
    #[serde(default)]
    pub events: Vec<CursorUsageEvent>,
}

impl CursorCache {
    pub(crate) fn load(path: &Path) -> Self {
        let Ok(bytes) = fs::read(path) else {
            return Self::default();
        };
        serde_json::from_slice::<Self>(&bytes).unwrap_or_default()
    }

    pub(crate) fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut copy = Self {
            version: CACHE_VERSION,
            sealed_from_ms: self.sealed_from_ms,
            sealed_to_ms: self.sealed_to_ms,
            events: self.events.clone(),
        };
        copy.events
            .sort_by(|a, b| a.timestamp.as_deref().cmp(&b.timestamp.as_deref()));
        fs::write(path, serde_json::to_vec(&copy)?)
    }

    pub(crate) fn is_populated(&self) -> bool {
        !self.events.is_empty()
    }
}

/// API windows that still need fetching for `[since, until]` at `now`.
///
/// History older than two days is skipped when the cache already sealed that
/// span. The last two days are always returned so recently mutated rows refresh.
pub(crate) fn needed_ranges(
    now_ms: i64,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    sealed_from_ms: Option<i64>,
    sealed_to_ms: Option<i64>,
) -> Vec<TimeRange> {
    let query_end = until_ms.unwrap_or(now_ms).min(now_ms);
    let query_start = since_ms.unwrap_or_else(|| now_ms.saturating_sub(DEFAULT_LOOKBACK_MS));
    if query_end <= query_start {
        return Vec::new();
    }
    let fresh_start = now_ms.saturating_sub(SEAL_AGE_MS);
    let mut ranges = Vec::new();

    let hist_end = query_end.min(fresh_start);
    if query_start < hist_end {
        ranges.extend(subtract_sealed(
            TimeRange {
                start_ms: query_start,
                end_ms: hist_end,
            },
            sealed_from_ms,
            sealed_to_ms,
        ));
    }

    let fresh_query_start = query_start.max(fresh_start);
    if fresh_query_start < query_end {
        ranges.push(TimeRange {
            start_ms: fresh_query_start,
            end_ms: query_end,
        });
    }
    merge_ranges(ranges)
}

fn subtract_sealed(
    need: TimeRange,
    sealed_from_ms: Option<i64>,
    sealed_to_ms: Option<i64>,
) -> Vec<TimeRange> {
    let (Some(sealed_from), Some(sealed_to)) = (sealed_from_ms, sealed_to_ms) else {
        return vec![need];
    };
    if sealed_to <= sealed_from || sealed_to <= need.start_ms || sealed_from >= need.end_ms {
        return vec![need];
    }
    let mut out = Vec::new();
    if need.start_ms < sealed_from {
        out.push(TimeRange {
            start_ms: need.start_ms,
            end_ms: sealed_from.min(need.end_ms),
        });
    }
    if sealed_to < need.end_ms {
        out.push(TimeRange {
            start_ms: sealed_to.max(need.start_ms),
            end_ms: need.end_ms,
        });
    }
    out
}

fn merge_ranges(mut ranges: Vec<TimeRange>) -> Vec<TimeRange> {
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort_by_key(|range| range.start_ms);
    let mut merged = vec![ranges[0]];
    for range in ranges.into_iter().skip(1) {
        let last = merged.last_mut().expect("merged starts non-empty");
        if range.start_ms <= last.end_ms {
            last.end_ms = last.end_ms.max(range.end_ms);
        } else {
            merged.push(range);
        }
    }
    merged
}

pub(crate) fn merge_events(
    cached: Vec<CursorUsageEvent>,
    fetched: Vec<CursorUsageEvent>,
    fetched_ranges: &[TimeRange],
) -> Vec<CursorUsageEvent> {
    use super::parser::{event_dedupe_key, event_timestamp_ms};

    let mut events: Vec<CursorUsageEvent> = cached
        .into_iter()
        .filter(|event| {
            let ts = event_timestamp_ms(event).as_millis();
            !fetched_ranges
                .iter()
                .any(|range| ts >= range.start_ms && ts < range.end_ms)
        })
        .collect();
    events.extend(fetched);
    let mut seen = std::collections::HashSet::new();
    events.retain(|event| seen.insert(event_dedupe_key(event)));
    events
}

pub(crate) fn advance_seal(
    cache: &mut CursorCache,
    now_ms: i64,
    query_start_ms: i64,
    fetched_ok: bool,
) {
    if !fetched_ok {
        return;
    }
    let fresh_start = now_ms.saturating_sub(SEAL_AGE_MS);
    cache.sealed_from_ms = Some(
        cache
            .sealed_from_ms
            .map_or(query_start_ms, |existing| existing.min(query_start_ms)),
    );
    cache.sealed_to_ms = Some(
        cache
            .sealed_to_ms
            .map_or(fresh_start, |existing| existing.max(fresh_start)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 60 * 60 * 1000;

    fn event(ts: i64, model: &str, input: u64) -> CursorUsageEvent {
        CursorUsageEvent {
            timestamp: Some(ts.to_string()),
            model: Some(model.to_string()),
            token_usage: Some(super::super::parser::CursorTokenUsage {
                input_tokens: input,
                output_tokens: 1,
                ..super::super::parser::CursorTokenUsage::default()
            }),
            ..CursorUsageEvent::default()
        }
    }

    #[test]
    fn empty_cache_fetches_the_default_lookback() {
        let now = 10_000 * DAY;
        let ranges = needed_ranges(now, None, None, None, None);
        assert_eq!(
            ranges,
            vec![TimeRange {
                start_ms: now - DEFAULT_LOOKBACK_MS,
                end_ms: now,
            }]
        );
    }

    #[test]
    fn sealed_history_only_refetches_the_last_two_days() {
        let now = 10_000 * DAY;
        let fresh = now - SEAL_AGE_MS;
        let ranges = needed_ranges(
            now,
            None,
            None,
            Some(now - DEFAULT_LOOKBACK_MS),
            Some(fresh),
        );
        assert_eq!(
            ranges,
            vec![TimeRange {
                start_ms: fresh,
                end_ms: now,
            }]
        );
    }

    #[test]
    fn since_before_the_seal_fetches_the_gap_and_the_fresh_window() {
        let now = 10_000 * DAY;
        let fresh = now - SEAL_AGE_MS;
        let sealed_from = now - 30 * DAY;
        let since = now - 60 * DAY;
        let ranges = needed_ranges(now, Some(since), None, Some(sealed_from), Some(fresh));
        assert_eq!(
            ranges,
            vec![
                TimeRange {
                    start_ms: since,
                    end_ms: sealed_from,
                },
                TimeRange {
                    start_ms: fresh,
                    end_ms: now,
                },
            ]
        );
    }

    #[test]
    fn fully_sealed_historical_query_hits_no_api() {
        let now = 10_000 * DAY;
        let fresh = now - SEAL_AGE_MS;
        let since = now - 40 * DAY;
        let until = now - 10 * DAY;
        let ranges = needed_ranges(
            now,
            Some(since),
            Some(until),
            Some(now - DEFAULT_LOOKBACK_MS),
            Some(fresh),
        );
        assert!(ranges.is_empty());
    }

    #[test]
    fn merge_replaces_events_inside_fetched_ranges() {
        let cached = vec![
            event(100, "k3", 1),
            event(200, "k3", 2),
            event(300, "k3", 3),
        ];
        let fetched = vec![event(200, "k3", 99)];
        let merged = merge_events(
            cached,
            fetched,
            &[TimeRange {
                start_ms: 150,
                end_ms: 250,
            }],
        );
        let inputs: Vec<u64> = merged
            .iter()
            .map(|event| event.token_usage.as_ref().unwrap().input_tokens)
            .collect();
        assert_eq!(inputs, vec![1, 3, 99]);
    }

    #[test]
    fn round_trips_cache_file() {
        let dir = ccusage_test_support::fs_fixture!({});
        let path = dir.path("events-v1.json");
        let cache = CursorCache {
            version: 1,
            sealed_from_ms: Some(1),
            sealed_to_ms: Some(2),
            events: vec![event(1, "k3", 4)],
        };
        cache.save(&path).unwrap();
        let loaded = CursorCache::load(&path);
        assert_eq!(loaded.sealed_from_ms, Some(1));
        assert_eq!(loaded.events.len(), 1);
        assert_eq!(
            loaded.events[0].token_usage.as_ref().unwrap().input_tokens,
            4
        );
    }
}
