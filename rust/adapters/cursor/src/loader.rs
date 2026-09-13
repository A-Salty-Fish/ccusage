use std::collections::HashSet;

use crate::{LoadedEntry, PricingMap, Result, cli::SharedArgs, debug_log, parse_tz};

use super::{
    cache::{
        CursorCache, DEFAULT_LOOKBACK_MS, TimeRange, advance_seal, merge_events, needed_ranges,
    },
    client::{LiveTransport, UsageTransport, fetch_range},
    parser::{CursorUsageEvent, usage_event_to_entry},
    paths::{cache_path, has_cache_file, has_local_credentials, resolved_access_token},
};

pub fn load_entries(shared: &SharedArgs, pricing: &PricingMap) -> Result<Vec<LoadedEntry>> {
    crate::progress::track_usage_load(
        crate::progress::UsageLoadAgent("Cursor"),
        shared.json,
        || load_entries_inner(shared, pricing, now_ms(), &LiveTransport),
    )
}

pub fn has_data() -> bool {
    has_local_credentials() || has_cache_file()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn load_entries_inner(
    shared: &SharedArgs,
    pricing: &PricingMap,
    now_ms: i64,
    transport: &dyn UsageTransport,
) -> Result<Vec<LoadedEntry>> {
    let tz = parse_tz(shared.timezone.as_deref());
    let cache_file = cache_path();
    let mut cache = cache_file
        .as_ref()
        .map(|path| CursorCache::load(path))
        .unwrap_or_default();
    let token = resolved_access_token();
    let (since_ms, until_ms) = crate::date_range_bounds_ms(
        shared.since.as_deref(),
        shared.until.as_deref(),
        tz.as_ref(),
    );
    let query_start = since_ms.unwrap_or_else(|| now_ms.saturating_sub(DEFAULT_LOOKBACK_MS));

    let events = if shared.offline || token.is_none() {
        cache.events
    } else {
        let ranges = needed_ranges(
            now_ms,
            since_ms,
            until_ms,
            cache.sealed_from_ms,
            cache.sealed_to_ms,
        );
        let token = token.expect("checked is_some");
        match fetch_all_ranges(transport, &token, &ranges, shared) {
            Ok(fetched) => {
                let merged = merge_events(cache.events, fetched, &ranges);
                cache.events = merged;
                advance_seal(&mut cache, now_ms, query_start, true);
                if let Some(path) = cache_file.as_ref()
                    && let Err(error) = cache.save(path)
                {
                    debug_log(
                        shared,
                        format!("Failed to write Cursor usage cache: {error}"),
                    );
                }
                cache.events
            }
            Err(error) => {
                if cache.is_populated() {
                    debug_log(
                        shared,
                        format!("Cursor API failed; using sealed cache: {error}"),
                    );
                    cache.events
                } else {
                    return Err(error);
                }
            }
        }
    };

    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for event in events {
        let Some(entry) = usage_event_to_entry(&event, tz.as_ref(), shared.mode, pricing) else {
            continue;
        };
        if !seen.insert(super::parser::event_dedupe_key(&event)) {
            continue;
        }
        entries.push(entry);
    }
    entries.sort_by_key(|entry| entry.timestamp);
    Ok(entries)
}

fn fetch_all_ranges(
    transport: &dyn UsageTransport,
    token: &str,
    ranges: &[TimeRange],
    shared: &SharedArgs,
) -> Result<Vec<CursorUsageEvent>> {
    let mut fetched = Vec::new();
    for range in ranges {
        debug_log(
            shared,
            format!("Fetching Cursor usage {}..{}", range.start_ms, range.end_ms),
        );
        fetched.extend(fetch_range(transport, token, range.start_ms, range.end_ms)?);
    }
    Ok(fetched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, sync::Mutex};

    use ccusage_test_support::{EnvVarsGuard, fs_fixture};

    use super::super::{
        cache::SEAL_AGE_MS,
        parser::{CursorTokenUsage, UsageEventsResponse},
        paths::{CACHE_ENV, TOKEN_ENV},
    };

    struct ScriptedTransport {
        pages: Mutex<Vec<UsageEventsResponse>>,
        calls: Mutex<Vec<(u32, i64, i64)>>,
    }

    impl ScriptedTransport {
        fn new(pages: Vec<UsageEventsResponse>) -> Self {
            Self {
                pages: Mutex::new(pages),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl UsageTransport for ScriptedTransport {
        fn fetch_page(
            &self,
            _token: &str,
            page: u32,
            start_ms: i64,
            end_ms: i64,
        ) -> Result<UsageEventsResponse> {
            self.calls.lock().unwrap().push((page, start_ms, end_ms));
            let mut pages = self.pages.lock().unwrap();
            if pages.is_empty() {
                return Ok(UsageEventsResponse::default());
            }
            Ok(pages.remove(0))
        }
    }

    fn sample_event(ts: i64, input: u64) -> CursorUsageEvent {
        CursorUsageEvent {
            timestamp: Some(ts.to_string()),
            model: Some("k3".to_string()),
            token_usage: Some(CursorTokenUsage {
                input_tokens: input,
                output_tokens: 1,
                total_cents: Some(10.0),
                ..CursorTokenUsage::default()
            }),
            ..CursorUsageEvent::default()
        }
    }

    fn page(events: Vec<CursorUsageEvent>) -> UsageEventsResponse {
        let total = events.len() as u64;
        UsageEventsResponse {
            usage_events_display: events,
            total_usage_events_count: total,
        }
    }

    #[test]
    fn second_load_only_refetches_the_unsealed_window() {
        let fixture = fs_fixture!({});
        let cache_file = fixture.path("cursor-cache.json");
        let _guard = EnvVarsGuard::set_many([
            (TOKEN_ENV, Some(OsString::from("test-token"))),
            (CACHE_ENV, Some(cache_file.clone().into_os_string())),
            ("HOME", Some(fixture.root().as_os_str().to_os_string())),
            (
                "USERPROFILE",
                Some(fixture.root().as_os_str().to_os_string()),
            ),
            (
                "APPDATA",
                Some(fixture.path("empty-appdata").into_os_string()),
            ),
        ]);
        let now = 1_780_000_000_000;
        let old_ts = now - SEAL_AGE_MS - 10_000;
        let fresh_ts = now - 3_600_000;
        let shared = SharedArgs::default();
        let pricing = PricingMap::load_embedded();

        let first = ScriptedTransport::new(vec![page(vec![
            sample_event(old_ts, 10),
            sample_event(fresh_ts, 20),
        ])]);
        let entries = load_entries_inner(&shared, &pricing, now, &first).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(first.calls.lock().unwrap().len(), 1);

        let second = ScriptedTransport::new(vec![page(vec![sample_event(fresh_ts, 21)])]);
        let entries = load_entries_inner(&shared, &pricing, now, &second).unwrap();
        let calls = second.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].1 >= now - SEAL_AGE_MS,
            "refetched start {}",
            calls[0].1
        );
        assert_eq!(entries.len(), 2);
        let fresh = entries
            .iter()
            .find(|entry| entry.timestamp.as_millis() == fresh_ts)
            .unwrap();
        assert_eq!(fresh.data.message.usage.input_tokens, 21);
        let sealed = entries
            .iter()
            .find(|entry| entry.timestamp.as_millis() == old_ts)
            .unwrap();
        assert_eq!(sealed.data.message.usage.input_tokens, 10);
    }

    #[test]
    fn offline_reads_the_cache_without_calling_the_transport() {
        let fixture = fs_fixture!({});
        let cache_file = fixture.path("cursor-cache.json");
        let cache = CursorCache {
            version: 1,
            sealed_from_ms: Some(1),
            sealed_to_ms: Some(2),
            events: vec![sample_event(1_750_000_000_000, 7)],
        };
        cache.save(&cache_file).unwrap();
        let _guard = EnvVarsGuard::set_many([
            (TOKEN_ENV, Some(OsString::from("test-token"))),
            (CACHE_ENV, Some(cache_file.into_os_string())),
        ]);
        let transport = ScriptedTransport::new(vec![page(vec![sample_event(9, 99)])]);
        let shared = SharedArgs {
            offline: true,
            ..SharedArgs::default()
        };
        let entries = load_entries_inner(
            &shared,
            &PricingMap::load_embedded(),
            1_780_000_000_000,
            &transport,
        )
        .unwrap();
        assert!(transport.calls.lock().unwrap().is_empty());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].data.message.usage.input_tokens, 7);
    }
}
