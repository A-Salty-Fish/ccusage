use std::time::Duration;

use serde_json::json;

use crate::{Result, cli_error};

use super::parser::UsageEventsResponse;

pub(crate) const USAGE_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetFilteredUsageEvents";
pub(crate) const PAGE_SIZE: u32 = 100;
pub(crate) const MAX_PAGES: u32 = 200;
const REQUEST_TIMEOUT_SECONDS: u64 = 30;
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) trait UsageTransport {
    fn fetch_page(
        &self,
        token: &str,
        page: u32,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<UsageEventsResponse>;
}

pub(crate) struct LiveTransport;

impl UsageTransport for LiveTransport {
    fn fetch_page(
        &self,
        token: &str,
        page: u32,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<UsageEventsResponse> {
        fetch_page(token, page, start_ms, end_ms)
    }
}

pub(crate) fn fetch_page(
    token: &str,
    page: u32,
    start_ms: i64,
    end_ms: i64,
) -> Result<UsageEventsResponse> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(REQUEST_TIMEOUT_SECONDS)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut response = agent
        .post(USAGE_URL)
        .header("Authorization", &format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("Connect-Protocol-Version", "1")
        .header("User-Agent", "ccusage-cursor-adapter")
        .send(
            json!({
                "startDate": start_ms.to_string(),
                "endDate": end_ms.to_string(),
                "page": page,
                "pageSize": PAGE_SIZE,
            })
            .to_string(),
        )
        .map_err(|error| cli_error(format!("Cursor usage request failed: {error}")))?;
    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        return Err(cli_error(format!(
            "Cursor usage request failed (HTTP {status}). The token may be expired; sign in to Cursor again or set CCUSAGE_CURSOR_TOKEN."
        )));
    }
    if status != 200 {
        return Err(cli_error(format!(
            "Cursor usage request failed (HTTP {status})."
        )));
    }
    let text = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .map_err(|error| cli_error(format!("Failed to read Cursor usage response: {error}")))?;
    serde_json::from_str(&text).map_err(|error| {
        let _ = error;
        cli_error("Failed to parse Cursor usage response.")
    })
}

pub(crate) fn fetch_range(
    transport: &dyn UsageTransport,
    token: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<super::parser::CursorUsageEvent>> {
    let mut events = Vec::new();
    let mut fetched = 0u64;
    let mut total: Option<u64> = None;
    for page in 1..=MAX_PAGES {
        let response = transport.fetch_page(token, page, start_ms, end_ms)?;
        let raw_count = response.usage_events_display.len() as u64;
        if page == 1 && response.total_usage_events_count > 0 {
            total = Some(response.total_usage_events_count);
        }
        events.extend(response.usage_events_display);
        fetched += raw_count;
        if raw_count == 0 {
            break;
        }
        if let Some(total) = total {
            if fetched >= total {
                break;
            }
        } else if raw_count < u64::from(PAGE_SIZE) {
            break;
        }
    }
    Ok(events)
}
