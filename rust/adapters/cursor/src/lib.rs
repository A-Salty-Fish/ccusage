use ccusage_adapter_common::filter_loaded_entries_by_date;
use ccusage_core::*;

mod cache;
mod client;
mod loader;
mod parser;
mod paths;
mod report;

use crate::{
    PricingMap, Result, UsageTableOptions, cli::AgentCommandArgs, print_json_or_jq,
    print_usage_table_with_options, sort_summaries, wants_json,
};

pub use loader::{has_data, load_entries};
pub(crate) use report::report_from_rows;
pub use report::summarize_entries;

pub fn run(args: AgentCommandArgs) -> Result<()> {
    let shared = args.shared;
    let pricing = PricingMap::load_with_overrides(
        shared.offline,
        crate::log_level() != Some(0),
        shared.pricing_overrides.iter(),
    );
    let mut entries = load_entries(&shared, &pricing)?;
    if entries.is_empty() && !paths::has_local_credentials() && !paths::has_cache_file() {
        return Err(paths::cli_token_error());
    }
    filter_loaded_entries_by_date(&mut entries, &shared);
    let mut rows = summarize_entries(&entries, args.kind)?;
    sort_summaries(&mut rows, &shared.order, |row| {
        ccusage_core::summary_period(row)
    });
    if wants_json(&shared) {
        return print_json_or_jq(
            report_from_rows(&rows, args.kind),
            shared.jq.as_deref(),
            shared.no_cost,
        );
    }
    let table_options = UsageTableOptions {
        show_cache_creation: rows.iter().any(|row| row.cache_creation_tokens > 0),
    };
    print_usage_table_with_options(
        "Cursor Token Usage Report",
        ccusage_core::first_column(args.kind),
        &rows,
        &shared,
        false,
        None,
        table_options,
    )?;
    Ok(())
}
