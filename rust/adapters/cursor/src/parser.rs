use std::sync::Arc;

use jiff::tz::TimeZone as JiffTimeZone;
use serde::{Deserialize, Serialize};

use crate::{
    LoadedEntry, PricingMap, TimestampMs, TokenUsageRaw, UsageEntry, UsageMessage,
    calculate_cost_for_usage_at, cli::CostMode, format_date_tz, format_rfc3339_millis,
    missing_pricing_model_for_usage,
};
use ccusage_adapter_common::jsonl;

/// One page of `GetFilteredUsageEvents`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsageEventsResponse {
    #[serde(default)]
    pub(crate) usage_events_display: Vec<CursorUsageEvent>,
    #[serde(default, deserialize_with = "jsonl::lenient_u64")]
    pub(crate) total_usage_events_count: u64,
}

/// A single account usage event as returned by the Cursor dashboard API.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CursorUsageEvent {
    #[serde(default, deserialize_with = "jsonl::non_empty_string")]
    pub(crate) timestamp: Option<String>,
    #[serde(default, deserialize_with = "jsonl::non_empty_string")]
    pub(crate) model: Option<String>,
    #[serde(default, deserialize_with = "jsonl::non_empty_string")]
    pub(crate) conversation_id: Option<String>,
    #[serde(default, deserialize_with = "jsonl::non_empty_string")]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) is_headless: Option<bool>,
    #[serde(default, deserialize_with = "jsonl::lenient_object")]
    pub(crate) token_usage: Option<CursorTokenUsage>,
    #[serde(default, deserialize_with = "jsonl::lenient_f64")]
    pub(crate) charged_cents: Option<f64>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CursorTokenUsage {
    #[serde(default, deserialize_with = "jsonl::lenient_u64")]
    pub(crate) input_tokens: u64,
    #[serde(default, deserialize_with = "jsonl::lenient_u64")]
    pub(crate) output_tokens: u64,
    #[serde(default, deserialize_with = "jsonl::lenient_u64")]
    pub(crate) cache_write_tokens: u64,
    #[serde(default, deserialize_with = "jsonl::lenient_u64")]
    pub(crate) cache_read_tokens: u64,
    #[serde(default, deserialize_with = "jsonl::lenient_f64")]
    pub(crate) total_cents: Option<f64>,
}

/// Convert one usage event into a `LoadedEntry`.
///
/// `tokenUsage.totalCents` is the dashboard's token-cost (it matches public
/// vendor list pricing for most events), so it is stored as `cost_usd`.
/// `chargedCents` — what the plan actually deducts — is surfaced through
/// `credits`. Cursor reports input and cache buckets separately; input is not
/// an OpenAI-style total that includes cache.
pub(crate) fn usage_event_to_entry(
    event: &CursorUsageEvent,
    tz: Option<&JiffTimeZone>,
    mode: CostMode,
    pricing: &PricingMap,
) -> Option<LoadedEntry> {
    let token_usage = event.token_usage.as_ref()?;
    let usage = TokenUsageRaw {
        input_tokens: token_usage.input_tokens,
        output_tokens: token_usage.output_tokens,
        cache_creation_input_tokens: token_usage.cache_write_tokens,
        cache_read_input_tokens: token_usage.cache_read_tokens,
        speed: None,
        cache_creation: None,
    };
    if usage.input_tokens == 0
        && usage.output_tokens == 0
        && usage.cache_creation_input_tokens == 0
        && usage.cache_read_input_tokens == 0
    {
        return None;
    }
    let timestamp = event_timestamp_ms(event);
    let model = event.model.clone();
    let cost_usd = token_usage.total_cents.map(|cents| cents / 100.0);
    let credits = event.charged_cents.map(|cents| cents / 100.0);
    let mut cost = calculate_cost_for_usage_at(
        model.as_deref(),
        usage,
        cost_usd,
        Some(timestamp),
        mode,
        Some(pricing),
    );
    let mut missing_pricing_model =
        missing_pricing_model_for_usage(model.as_deref(), usage, cost_usd, mode, Some(pricing));
    // Grok Bot rows (`grok-bot-default` / automation / cua) have no published
    // per-token rate. Cursor still records totalCents; use that instead of
    // dropping the model from calculate-mode totals.
    if missing_pricing_model.is_some()
        && let Some(recorded) = cost_usd
    {
        cost = recorded;
        missing_pricing_model = None;
    }
    let session_id = event
        .conversation_id
        .clone()
        .or_else(|| model.clone())
        .unwrap_or_else(|| "cursor".to_string());
    let timestamp_text = format_rfc3339_millis(timestamp);
    let data = UsageEntry {
        session_id: Some(session_id.clone()),
        timestamp: timestamp_text,
        version: None,
        message: UsageMessage {
            usage,
            model: model.clone(),
            id: None,
        },
        cost_usd,
        request_id: None,
        is_api_error_message: None,
        is_sidechain: None,
    };
    Some(LoadedEntry {
        date: format_date_tz(timestamp, tz),
        timestamp,
        project: Arc::from("cursor"),
        session_id: Arc::from(session_id),
        project_path: Arc::from("Cursor"),
        cost,
        extra_total_tokens: 0,
        credits,
        message_count: None,
        model,
        usage_limit_reset_time: None,
        missing_pricing_model,
        data,
    })
}

pub(crate) fn event_timestamp_ms(event: &CursorUsageEvent) -> TimestampMs {
    TimestampMs::from_millis(parse_timestamp_millis(event.timestamp.as_deref()).unwrap_or(0))
}

pub(crate) fn event_dedupe_key(event: &CursorUsageEvent) -> String {
    let usage = event.token_usage.clone().unwrap_or_default();
    format!(
        "{}|{}|{}|{}|{}|{}|{}",
        event.timestamp.as_deref().unwrap_or_default(),
        event.conversation_id.as_deref().unwrap_or_default(),
        event.model.as_deref().unwrap_or_default(),
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_write_tokens,
        usage.cache_read_tokens,
    )
}

fn parse_timestamp_millis(value: Option<&str>) -> Option<i64> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if let Ok(ms) = value.parse::<i64>() {
        return (ms > 0).then_some(ms);
    }
    crate::parse_ts_timestamp(value).map(|ts| ts.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::PricingOverride;

    fn event(value: serde_json::Value) -> CursorUsageEvent {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn maps_token_usage_and_token_cost() {
        let tz = crate::parse_tz(Some("UTC"));
        let entry = usage_event_to_entry(
            &event(serde_json::json!({
                "timestamp": "1782261704029",
                "model": "claude-opus-4-8-thinking-max",
                "conversationId": "conv-1",
                "tokenUsage": {
                    "inputTokens": 4,
                    "outputTokens": 3834,
                    "cacheWriteTokens": 10182,
                    "cacheReadTokens": 518921,
                    "totalCents": 41.8968
                },
                "chargedCents": 4
            })),
            tz.as_ref(),
            CostMode::Display,
            &PricingMap::load_embedded(),
        )
        .unwrap();

        assert_eq!(entry.date, "2026-06-24");
        assert_eq!(entry.model.as_deref(), Some("claude-opus-4-8-thinking-max"));
        assert_eq!(entry.session_id.as_ref(), "conv-1");
        assert_eq!(entry.data.message.usage.input_tokens, 4);
        assert_eq!(entry.data.message.usage.output_tokens, 3834);
        assert_eq!(entry.data.message.usage.cache_creation_input_tokens, 10182);
        assert_eq!(entry.data.message.usage.cache_read_input_tokens, 518_921);
        assert!((entry.cost - 0.418968).abs() < 1e-9);
        assert_eq!(entry.credits, Some(0.04));
    }

    #[test]
    fn skips_events_without_token_counts() {
        assert!(
            usage_event_to_entry(
                &event(serde_json::json!({
                    "timestamp": "1782261704029",
                    "model": "default",
                    "tokenUsage": { "totalCents": 0 }
                })),
                None,
                CostMode::Display,
                &PricingMap::load_embedded(),
            )
            .is_none()
        );
    }

    #[test]
    fn parses_page_envelope() {
        let response: UsageEventsResponse = serde_json::from_value(serde_json::json!({
            "usageEventsDisplay": [
                {
                    "timestamp": "1782261704029",
                    "model": "gpt-5.5-extra-high",
                    "tokenUsage": {
                        "inputTokens": 318828,
                        "outputTokens": 32209,
                        "cacheReadTokens": 4009984,
                        "totalCents": 456.54
                    },
                    "chargedCents": 24
                }
            ],
            "totalUsageEventsCount": 1
        }))
        .unwrap();

        assert_eq!(response.usage_events_display.len(), 1);
        assert_eq!(response.total_usage_events_count, 1);
        let entry = usage_event_to_entry(
            &response.usage_events_display[0],
            None,
            CostMode::Display,
            &PricingMap::load_embedded(),
        )
        .unwrap();
        assert_eq!(entry.data.message.usage.input_tokens, 318_828);
        assert_eq!(entry.data.message.usage.cache_read_input_tokens, 4_009_984);
        assert!((entry.cost - 4.5654).abs() < 1e-9);
    }

    #[test]
    fn calculate_mode_uses_recorded_cents_when_the_model_is_unpriced() {
        let entry = usage_event_to_entry(
            &event(serde_json::json!({
                "timestamp": "1782261704029",
                "model": "grok-bot-default",
                "tokenUsage": {
                    "inputTokens": 100,
                    "outputTokens": 20,
                    "totalCents": 12.5
                }
            })),
            None,
            CostMode::Calculate,
            &PricingMap::load_embedded(),
        )
        .unwrap();
        assert!((entry.cost - 0.125).abs() < 1e-9);
        assert_eq!(entry.missing_pricing_model, None);
    }

    #[test]
    fn calculate_mode_uses_pricing_overrides() {
        let model = "k3".to_string();
        let pricing = PricingMap::load_with_overrides(
            true,
            false,
            [(
                &model,
                &PricingOverride {
                    input_cost_per_token: Some(1.0),
                    output_cost_per_token: Some(2.0),
                    ..PricingOverride::default()
                },
            )],
        );
        let entry = usage_event_to_entry(
            &event(serde_json::json!({
                "timestamp": "1782261704029",
                "model": "k3",
                "tokenUsage": {
                    "inputTokens": 10,
                    "outputTokens": 5,
                    "totalCents": 99
                }
            })),
            None,
            CostMode::Calculate,
            &pricing,
        )
        .unwrap();
        assert_eq!(entry.cost, 20.0);
        assert_eq!(entry.data.cost_usd, Some(0.99));
    }

    #[test]
    fn session_falls_back_to_model_when_conversation_id_is_absent() {
        let entry = usage_event_to_entry(
            &event(serde_json::json!({
                "timestamp": "1000",
                "model": "k3",
                "tokenUsage": { "inputTokens": 1, "outputTokens": 1 }
            })),
            None,
            CostMode::Display,
            &PricingMap::load_embedded(),
        )
        .unwrap();
        assert_eq!(entry.session_id.as_ref(), "k3");
    }
}
