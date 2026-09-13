use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::{
    BucketKind, LoadedEntry, Result, SessionAccumulator,
    cli::{AgentReportKind, WeekDay},
    summarize_by_key, summarize_summaries_by_bucket, totals_json,
};

pub(crate) fn report_from_rows(rows: &[crate::UsageSummary], kind: AgentReportKind) -> Value {
    let rows_json = rows
        .iter()
        .map(|row| ccusage_core::agent_summary_json(row, kind, kind == AgentReportKind::Session))
        .collect::<Vec<_>>();
    json!({
        rows_key(kind): rows_json,
        "totals": totals_json(rows),
    })
}

pub fn summarize_entries(
    entries: &[LoadedEntry],
    kind: AgentReportKind,
) -> Result<Vec<crate::UsageSummary>> {
    match kind {
        AgentReportKind::Daily => summarize_by_key(
            entries,
            |entry| entry.date.clone(),
            |date| (date.to_string(), None),
        ),
        AgentReportKind::Monthly => {
            let daily = summarize_entries(entries, AgentReportKind::Daily)?;
            Ok(summarize_summaries_by_bucket(
                &daily,
                BucketKind::Monthly,
                WeekDay::Sunday,
            ))
        }
        AgentReportKind::Session => {
            let mut groups = BTreeMap::<String, SessionAccumulator>::new();
            for entry in entries {
                groups
                    .entry(entry.session_id.to_string())
                    .or_default()
                    .add_entry(entry);
            }
            groups
                .into_values()
                .map(SessionAccumulator::into_summary)
                .collect()
        }
        AgentReportKind::Weekly => {
            let daily = summarize_entries(entries, AgentReportKind::Daily)?;
            Ok(summarize_summaries_by_bucket(
                &daily,
                BucketKind::Weekly,
                WeekDay::Sunday,
            ))
        }
    }
}

fn rows_key(kind: AgentReportKind) -> &'static str {
    match kind {
        AgentReportKind::Daily => "daily",
        AgentReportKind::Weekly => "weekly",
        AgentReportKind::Monthly => "monthly",
        AgentReportKind::Session => "sessions",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TimestampMs, TokenUsageRaw, UsageEntry, UsageMessage, format_rfc3339_millis};
    use std::sync::Arc;

    fn entry(session_id: &str, date: &str, millis: i64) -> LoadedEntry {
        let timestamp = TimestampMs::from_millis(millis);
        LoadedEntry {
            data: UsageEntry {
                session_id: Some(session_id.to_string()),
                timestamp: format_rfc3339_millis(timestamp),
                version: None,
                message: UsageMessage {
                    usage: TokenUsageRaw {
                        input_tokens: 10,
                        output_tokens: 2,
                        cache_creation_input_tokens: 4,
                        cache_read_input_tokens: 8,
                        speed: None,
                        cache_creation: None,
                    },
                    model: Some("k3".to_string()),
                    id: Some(format!("usage-{millis}")),
                },
                cost_usd: Some(0.5),
                request_id: None,
                is_api_error_message: None,
                is_sidechain: None,
            },
            timestamp,
            date: date.to_string(),
            project: Arc::from("cursor"),
            session_id: Arc::from(session_id),
            project_path: Arc::from("Cursor"),
            cost: 0.5,
            extra_total_tokens: 0,
            credits: Some(0.1),
            message_count: None,
            model: Some("k3".to_string()),
            usage_limit_reset_time: None,
            missing_pricing_model: None,
        }
    }

    #[test]
    fn snapshots_focused_cursor_json_reports() {
        let entries = [
            entry("conv-a", "2026-06-24", 1_782_261_704_029),
            entry("conv-b", "2026-06-24", 1_782_261_804_029),
        ];
        let daily = summarize_entries(&entries, AgentReportKind::Daily).unwrap();
        let monthly = summarize_entries(&entries, AgentReportKind::Monthly).unwrap();
        let session = summarize_entries(&entries, AgentReportKind::Session).unwrap();

        insta::assert_json_snapshot!(
            "focused_cursor_daily_json",
            report_from_rows(&daily, AgentReportKind::Daily)
        );
        insta::assert_json_snapshot!(
            "focused_cursor_monthly_json",
            report_from_rows(&monthly, AgentReportKind::Monthly)
        );
        insta::assert_json_snapshot!(
            "focused_cursor_session_json",
            report_from_rows(&session, AgentReportKind::Session)
        );
    }
}
