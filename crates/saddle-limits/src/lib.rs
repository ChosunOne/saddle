//! runtime resource budgets, never capability grants (D145).

use serde::{Deserialize, Serialize};
use std::cell::RefCell;

macro_rules! budgets {
    ($($name:ident : $ty:ty = $default:expr),* $(,)?) => {
        /// host-owned resource settings; no field grants an operation.
        #[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
        #[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
        pub struct Budgets { $(/// a configurable resource/presentation budget.
            pub $name: $ty,)* }
        /// one source of defaults for the host, loader and standard guests.
        pub const DEFAULTS: Budgets = Budgets { $($name: $default,)* };
        impl Default for Budgets {
            fn default() -> Self { DEFAULTS.clone() }
        }
        impl Budgets {
            /// reject zero/unrepresentable budgets instead of wrapping at the guest ABI.
            pub fn validate(&self) -> Result<(), String> {
                $(if (self.$name == 0 && !matches!(stringify!($name), "http_redirects" | "exec_report_paths" | "exec_report_denials" | "web_quotes" | "image_count")) || (stringify!($ty) == "usize" && self.$name as u64 > u64::from(u32::MAX)) {
                    return Err(format!("tool-limits.{} must be positive (except disabled report/redirect/image counts) and fit its ABI representation", stringify!($name).replace('_', "-")));
                })*
                Ok(())
            }
        }
        $(/// read a budget in the current synchronous operation.
        pub fn $name() -> $ty { CURRENT.with(|value| value.borrow().$name) })*
    }
}

budgets! {
    plugin_component_bytes: usize = 4 * 1024 * 1024,
    plugin_manifest_bytes: usize = 16 * 1024,
    plugin_count: usize = 16,
    contribution_bytes: usize = 16 * 1024,
    contribution_count: usize = 32,
    persist_key_bytes: usize = 256,
    persist_value_bytes: usize = 64 * 1024,
    persist_keys: usize = 1024,
    persist_total_bytes: usize = 4 * 1024 * 1024,
    persist_changes: usize = 32,
    team_workers: usize = 8,
    team_steps: usize = 16,
    model_input_bytes: usize = 64 * 1024,
    model_output_tokens: usize = 4096,
    model_reply_bytes: usize = 64 * 1024,
    ui_bytes: usize = 16 * 1024,
    ui_widgets: usize = 32,
    ui_rows: usize = 12,
    event_queue: usize = 64,
    event_name_bytes: usize = 128,
    event_seconds: usize = 30,
    event_deliveries: usize = 128,
    event_bytes: usize = 16 * 1024,
    event_subscriptions: usize = 64,
    plugin_description_chars: usize = 1000,
    plugin_schema_bytes: usize = 16 * 1024,
    plugin_version_bytes: usize = 64,
    plugin_stack_bytes: usize = 512 * 1024,
    plugin_table_elements: usize = 10_000,
    plugin_instances: usize = 32,
    plugin_memories: usize = 4,
    plugin_tables: usize = 8,
    plugin_resources: usize = 128,
    plugin_random_bytes: usize = 64 * 1024,
    plugin_memory_bytes: usize = 16 * 1024 * 1024,
    plugin_fuel: u64 = 100_000_000,
    plugin_timeout_ms: usize = 2000,
    plugin_input_bytes: usize = 64 * 1024,
    plugin_output_bytes: usize = 64 * 1024,
    host_wait_seconds: usize = 300,
    host_calls: usize = 16,
    metadata_bytes: usize = 64 * 1024,
    path_bytes: usize = 4096,
    directory_name_bytes: usize = 1024,
    url_bytes: usize = 8192,
    header_value_bytes: usize = 8192,
    exec_arguments: usize = 128,
    exec_argument_bytes: usize = 64 * 1024,
    range_bytes: usize = 64 * 1024,
    directory_page_entries: usize = 128,
    directory_scan_entries: usize = 100_000,
    mutation_bytes: usize = 1024 * 1024,
    mutation_changes: usize = 32,
    diff_bytes: usize = 64 * 1024,
    diff_present_bytes: usize = 60 * 1024,
    native_diff_input_bytes: usize = 2 * 1024 * 1024,
    image_bytes: usize = 5 * 1024 * 1024,
    image_count: usize = 1,
    image_name_bytes: usize = 256,
    request_image_bytes: usize = 32 * 1024 * 1024,
    read_page_bytes: usize = 50 * 1024,
    read_page_lines: usize = 2000,
    read_scan_bytes: usize = 64 * 1024 * 1024,
    list_entries: usize = 500,
    search_visited: usize = 100_000,
    search_pending: usize = 2048,
    search_path_bytes: usize = 1024 * 1024,
    search_file_bytes: usize = 4 * 1024 * 1024,
    search_ignore_file_bytes: usize = 64 * 1024,
    search_ignore_bytes: usize = 128 * 1024,
    find_results: usize = 1000,
    grep_matches: usize = 200,
    grep_line_bytes: usize = 300,
    search_regex_bytes: usize = 1024 * 1024,
    text_bytes: usize = 50 * 1024,
    exec_capture_bytes: usize = 256 * 1024,
    exec_present_bytes: usize = 24 * 1024,
    exec_report_paths: usize = 16,
    exec_report_denials: usize = 8,
    exec_seconds: usize = 300,
    exec_tasks: usize = 16,
    presentation_bytes: usize = 60 * 1024,
    http_capture_bytes: usize = 100 * 1024,
    network_seconds: usize = 30,
    http_redirects: usize = 5,
    http_headers: usize = 64,
    http_header_bytes: usize = 64 * 1024,
    search_response_bytes: usize = 1024 * 1024,
    web_query_bytes: usize = 16 * 1024,
    web_results: usize = 100,
    web_default_results: usize = 5,
    web_quote_chars: usize = 1000,
    web_quotes: usize = 3,
    search_cache_entries: usize = 17,
    query_bytes: usize = 16 * 1024,
    query_result_bytes: usize = 50 * 1024,
    query_rows: usize = 100,
    query_value_bytes: usize = 1024 * 1024,
    query_columns: usize = 32,
    query_expr_depth: usize = 100,
    query_program_ops: usize = 100_000,
    query_steps: usize = 1_000_000,
}

thread_local! { static CURRENT: RefCell<Budgets> = RefCell::new(Budgets::default()); }

/// snapshot the current resource context; it carries no authority or credentials.
pub fn current() -> Budgets {
    CURRENT.with(|value| value.borrow().clone())
}

/// install a context for one synchronous operation and restore it, including on unwind.
pub fn using<T>(budgets: &Budgets, operation: impl FnOnce() -> T) -> T {
    let previous = CURRENT.with(|value| value.replace(budgets.clone()));
    let _restore = Restore(previous);
    operation()
}

/// initialize a fresh guest instance from its host's immutable invocation snapshot.
pub fn initialize(budgets: Budgets) {
    CURRENT.with(|value| {
        value.replace(budgets);
    });
}

struct Restore(Budgets);
impl Drop for Restore {
    fn drop(&mut self) {
        CURRENT.with(|value| {
            value.replace(self.0.clone());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn contexts_nest_restore_and_validate() {
        let original = current();
        let raised = Budgets {
            mutation_bytes: 8 * 1024 * 1024,
            ..original.clone()
        };
        using(&raised, || {
            assert_eq!(mutation_bytes(), raised.mutation_bytes);
            using(&original, || {
                assert_eq!(mutation_bytes(), original.mutation_bytes)
            });
            assert_eq!(mutation_bytes(), raised.mutation_bytes);
        });
        assert_eq!(current(), original);
        assert!(
            Budgets {
                host_calls: 0,
                ..original
            }
            .validate()
            .is_err()
        );
    }
}
