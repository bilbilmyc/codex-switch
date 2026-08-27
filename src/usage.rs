use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const UNKNOWN_MODEL: &str = "Unknown";
const DAILY_HISTORY_DAYS: i64 = 14;
const MAX_JSONL_LINE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsagePeriod {
    Today,
    Last7Days,
    Last30Days,
}

/// A closed-open time range in which the legacy shared provider can be attributed to one
/// profile. The range is measured in Unix milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyUsageWindow {
    pub start_unix_ms: u64,
    pub end_exclusive_unix_ms: u64,
}

impl LegacyUsageWindow {
    pub const fn new(start_unix_ms: u64, end_exclusive_unix_ms: u64) -> Self {
        Self {
            start_unix_ms,
            end_exclusive_unix_ms,
        }
    }

    fn contains(self, timestamp_unix_ms: u64) -> bool {
        self.start_unix_ms <= timestamp_unix_ms && timestamp_unix_ms < self.end_exclusive_unix_ms
    }
}

/// A normalized local usage query. New provider IDs are always exact; legacy shared-provider
/// records are only eligible inside the supplied windows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageScope {
    exact_provider_id: Option<String>,
    legacy_provider_id: Option<String>,
    legacy_windows: Vec<LegacyUsageWindow>,
    known_legacy_windows: Vec<LegacyUsageWindow>,
}

impl UsageScope {
    pub const fn all() -> Self {
        Self {
            exact_provider_id: None,
            legacy_provider_id: None,
            legacy_windows: Vec::new(),
            known_legacy_windows: Vec::new(),
        }
    }

    pub fn exact(provider_id: impl Into<String>) -> Self {
        Self {
            exact_provider_id: Some(provider_id.into()),
            legacy_provider_id: None,
            legacy_windows: Vec::new(),
            known_legacy_windows: Vec::new(),
        }
    }

    pub fn profile(
        exact_provider_id: impl Into<String>,
        legacy_provider_id: impl Into<String>,
        legacy_windows: Vec<LegacyUsageWindow>,
        known_legacy_windows: Vec<LegacyUsageWindow>,
    ) -> Self {
        Self {
            exact_provider_id: Some(exact_provider_id.into()),
            legacy_provider_id: Some(legacy_provider_id.into()),
            legacy_windows: normalize_windows(legacy_windows),
            known_legacy_windows: normalize_windows(known_legacy_windows),
        }
    }

    pub fn from_provider_filter(provider_filter: Option<&str>) -> Self {
        provider_filter.map_or_else(Self::all, Self::exact)
    }

    pub fn provider_filter(&self) -> Option<&str> {
        self.exact_provider_id.as_deref()
    }

    fn legacy_window_contains(&self, timestamp_unix_ms: u64) -> bool {
        self.legacy_windows
            .iter()
            .copied()
            .any(|window| window.contains(timestamp_unix_ms))
    }

    fn known_legacy_window_contains(&self, timestamp_unix_ms: u64) -> bool {
        self.known_legacy_windows
            .iter()
            .copied()
            .any(|window| window.contains(timestamp_unix_ms))
    }
}

fn normalize_windows(mut windows: Vec<LegacyUsageWindow>) -> Vec<LegacyUsageWindow> {
    windows.retain(|window| window.start_unix_ms < window.end_exclusive_unix_ms);
    windows.sort_by_key(|window| (window.start_unix_ms, window.end_exclusive_unix_ms));
    let mut normalized: Vec<LegacyUsageWindow> = Vec::with_capacity(windows.len());
    for window in windows {
        if let Some(previous) = normalized.last_mut()
            && window.start_unix_ms <= previous.end_exclusive_unix_ms
        {
            previous.end_exclusive_unix_ms = previous
                .end_exclusive_unix_ms
                .max(window.end_exclusive_unix_ms);
        } else {
            normalized.push(window);
        }
    }
    normalized
}

impl UsagePeriod {
    pub const fn day_count(self) -> i64 {
        match self {
            Self::Today => 1,
            Self::Last7Days => 7,
            Self::Last30Days => 30,
        }
    }

    const fn csv_label(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::Last7Days => "last_7_days",
            Self::Last30Days => "last_30_days",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageDateRange {
    pub start: NaiveDate,
    pub end_exclusive: NaiveDate,
}

impl UsageDateRange {
    pub fn contains(self, date: NaiveDate) -> bool {
        date >= self.start && date < self.end_exclusive
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub calls: u64,
}

impl TokenUsage {
    pub fn total_tokens(self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    fn add_assign_saturating(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(other.cache_write_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(other.reasoning_output_tokens);
        self.calls = self.calls.saturating_add(other.calls);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyUsage {
    pub date: NaiveDate,
    pub usage: TokenUsage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub model: String,
    pub usage: TokenUsage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatestContextUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
    pub model_context_window: u64,
    pub model: String,
    pub timestamp: String,
    pub cwd: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    pub period: UsagePeriod,
    pub provider_filter: Option<String>,
    pub collected_at: String,
    pub current_range: UsageDateRange,
    pub previous_range: UsageDateRange,
    pub current: TokenUsage,
    pub previous: TokenUsage,
    pub daily: Vec<DailyUsage>,
    pub model_distribution: Vec<ModelUsage>,
    pub latest_context: Option<LatestContextUsage>,
    /// Legacy shared-provider usage that cannot be proven to belong to any profile.
    pub unattributed_legacy: TokenUsage,
    pub skipped_lines: u64,
    pub skipped_files: u64,
}

impl UsageReport {
    pub fn model_distribution_csv(&self) -> String {
        usage_report_csv(self)
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum UsageError {
    #[error("the requested usage date range is outside chrono's supported range")]
    DateRangeOutOfBounds,
    #[error("no existing Codex session root could be read")]
    SessionRootsUnreadable,
}

pub fn collect_usage(
    sessions_dir: &Path,
    archived_sessions_dir: &Path,
    period: UsagePeriod,
    provider_filter: Option<&str>,
) -> Result<UsageReport, UsageError> {
    let scope = UsageScope::from_provider_filter(provider_filter);
    collect_usage_scoped(sessions_dir, archived_sessions_dir, period, &scope)
}

pub fn collect_usage_scoped(
    sessions_dir: &Path,
    archived_sessions_dir: &Path,
    period: UsagePeriod,
    scope: &UsageScope,
) -> Result<UsageReport, UsageError> {
    collect_usage_scoped_at(
        sessions_dir,
        archived_sessions_dir,
        period,
        scope,
        Local::now(),
    )
}

pub fn model_distribution_csv(models: &[ModelUsage]) -> String {
    let mut csv = String::from(
        "model,input_tokens,cached_input_tokens,cache_write_input_tokens,output_tokens,reasoning_output_tokens,calls\n",
    );
    for entry in models {
        push_csv_field(&mut csv, &entry.model);
        let _ = writeln!(
            csv,
            ",{},{},{},{},{},{}",
            entry.usage.input_tokens,
            entry.usage.cached_input_tokens,
            entry.usage.cache_write_input_tokens,
            entry.usage.output_tokens,
            entry.usage.reasoning_output_tokens,
            entry.usage.calls,
        );
    }
    csv
}

fn usage_report_csv(report: &UsageReport) -> String {
    let mut csv = String::from(
        "section,label,value,range_start,range_end_exclusive,collected_at,provider_filter,input_tokens,cached_input_tokens,cache_write_input_tokens,output_tokens,reasoning_output_tokens,calls\n",
    );
    let provider = report.provider_filter.as_deref().unwrap_or("all");
    push_metadata_csv_row(&mut csv, "period", report.period.csv_label());
    push_metadata_csv_row(&mut csv, "provider_filter", provider);
    push_metadata_csv_row(&mut csv, "collected_at", &report.collected_at);
    push_metadata_csv_row(
        &mut csv,
        "comparison_rule",
        "previous period through the same elapsed local time",
    );
    push_usage_csv_row(
        &mut csv,
        "summary",
        "current",
        report.current_range.start,
        report.current_range.end_exclusive,
        &report.collected_at,
        provider,
        report.current,
    );
    push_usage_csv_row(
        &mut csv,
        "summary",
        "previous_comparable",
        report.previous_range.start,
        report.previous_range.end_exclusive,
        &report.collected_at,
        provider,
        report.previous,
    );
    if report.unattributed_legacy != TokenUsage::default() {
        push_usage_csv_row(
            &mut csv,
            "summary",
            "legacy_unattributed",
            report.current_range.start,
            report.current_range.end_exclusive,
            &report.collected_at,
            provider,
            report.unattributed_legacy,
        );
    }
    for day in &report.daily {
        let Some(end_exclusive) = day.date.succ_opt() else {
            continue;
        };
        push_usage_csv_row(
            &mut csv,
            "daily",
            &day.date.to_string(),
            day.date,
            end_exclusive,
            &report.collected_at,
            provider,
            day.usage,
        );
    }
    for model in &report.model_distribution {
        push_usage_csv_row(
            &mut csv,
            "model",
            &model.model,
            report.current_range.start,
            report.current_range.end_exclusive,
            &report.collected_at,
            provider,
            model.usage,
        );
    }
    csv
}

fn push_metadata_csv_row(output: &mut String, label: &str, value: &str) {
    push_csv_field(output, "metadata");
    output.push(',');
    push_csv_field(output, label);
    output.push(',');
    push_csv_field(output, value);
    output.push_str(",,,,,,,,,,\n");
}

#[allow(clippy::too_many_arguments)]
fn push_usage_csv_row(
    output: &mut String,
    section: &str,
    label: &str,
    start: NaiveDate,
    end_exclusive: NaiveDate,
    collected_at: &str,
    provider: &str,
    usage: TokenUsage,
) {
    let start = start.to_string();
    let end_exclusive = end_exclusive.to_string();
    for (index, field) in [
        section,
        label,
        "",
        start.as_str(),
        end_exclusive.as_str(),
        collected_at,
        provider,
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            output.push(',');
        }
        push_csv_field(output, field);
    }
    let _ = writeln!(
        output,
        ",{},{},{},{},{},{}",
        usage.input_tokens,
        usage.cached_input_tokens,
        usage.cache_write_input_tokens,
        usage.output_tokens,
        usage.reasoning_output_tokens,
        usage.calls,
    );
}

#[cfg(test)]
fn collect_usage_at<Tz: TimeZone>(
    sessions_dir: &Path,
    archived_sessions_dir: &Path,
    period: UsagePeriod,
    provider_filter: Option<&str>,
    now: DateTime<Tz>,
) -> Result<UsageReport, UsageError> {
    let scope = UsageScope::from_provider_filter(provider_filter);
    collect_usage_scoped_at(sessions_dir, archived_sessions_dir, period, &scope, now)
}

fn collect_usage_scoped_at<Tz: TimeZone>(
    sessions_dir: &Path,
    archived_sessions_dir: &Path,
    period: UsagePeriod,
    scope: &UsageScope,
    now: DateTime<Tz>,
) -> Result<UsageReport, UsageError> {
    let today = now.date_naive();
    let windows = UsageWindows::new(today, now.time(), period)?;
    let collected_at = now.fixed_offset().to_rfc3339();
    let timezone = now.timezone();
    let mut accumulator = UsageAccumulator::new(period, scope, collected_at, windows)?;

    let sessions_status = scan_root(sessions_dir, scope, &timezone, &mut accumulator);
    let mut root_statuses = vec![sessions_status];
    if archived_sessions_dir != sessions_dir {
        root_statuses.push(scan_root(
            archived_sessions_dir,
            scope,
            &timezone,
            &mut accumulator,
        ));
    }
    if !root_statuses.contains(&RootScanStatus::Readable)
        && root_statuses.contains(&RootScanStatus::Failed)
    {
        return Err(UsageError::SessionRootsUnreadable);
    }

    Ok(accumulator.finish())
}

#[derive(Clone, Copy, Debug)]
struct UsageWindows {
    current: UsageDateRange,
    previous: UsageDateRange,
    daily: UsageDateRange,
    previous_cutoff: NaiveDateTime,
}

impl UsageWindows {
    fn new(
        today: NaiveDate,
        local_time: chrono::NaiveTime,
        period: UsagePeriod,
    ) -> Result<Self, UsageError> {
        let tomorrow = checked_shift(today, 1)?;
        let current_start = checked_shift(tomorrow, -period.day_count())?;
        let previous_start = checked_shift(current_start, -period.day_count())?;
        let previous_cutoff = checked_shift(current_start, -1)?.and_time(local_time);
        let daily_start = checked_shift(tomorrow, -DAILY_HISTORY_DAYS)?;

        Ok(Self {
            current: UsageDateRange {
                start: current_start,
                end_exclusive: tomorrow,
            },
            previous: UsageDateRange {
                start: previous_start,
                end_exclusive: current_start,
            },
            daily: UsageDateRange {
                start: daily_start,
                end_exclusive: tomorrow,
            },
            previous_cutoff,
        })
    }

    fn previous_contains(self, local_timestamp: NaiveDateTime) -> bool {
        self.previous.contains(local_timestamp.date()) && local_timestamp <= self.previous_cutoff
    }
}

fn checked_shift(date: NaiveDate, days: i64) -> Result<NaiveDate, UsageError> {
    date.checked_add_signed(Duration::days(days))
        .ok_or(UsageError::DateRangeOutOfBounds)
}

struct UsageAccumulator {
    period: UsagePeriod,
    scope: UsageScope,
    collected_at: String,
    windows: UsageWindows,
    current: TokenUsage,
    previous: TokenUsage,
    daily: BTreeMap<NaiveDate, TokenUsage>,
    models: BTreeMap<String, TokenUsage>,
    latest: Option<LatestCandidate>,
    unattributed_legacy: TokenUsage,
    seen_file_identities: BTreeSet<FileIdentity>,
    seen_session_ids: BTreeSet<String>,
    skipped_lines: u64,
    skipped_files: u64,
}

struct UsageEvent<'a> {
    occurred_at: DateTime<FixedOffset>,
    local_date: NaiveDate,
    local_timestamp: NaiveDateTime,
    timestamp: String,
    model: &'a str,
    cwd: Option<&'a Path>,
    raw_usage: RawTokenUsage,
    model_context_window: u64,
    count_usage: bool,
}

impl UsageAccumulator {
    fn new(
        period: UsagePeriod,
        scope: &UsageScope,
        collected_at: String,
        windows: UsageWindows,
    ) -> Result<Self, UsageError> {
        let mut daily = BTreeMap::new();
        let mut date = windows.daily.start;
        while date < windows.daily.end_exclusive {
            daily.insert(date, TokenUsage::default());
            date = checked_shift(date, 1)?;
        }

        Ok(Self {
            period,
            scope: scope.clone(),
            collected_at,
            windows,
            current: TokenUsage::default(),
            previous: TokenUsage::default(),
            daily,
            models: BTreeMap::new(),
            latest: None,
            unattributed_legacy: TokenUsage::default(),
            seen_file_identities: BTreeSet::new(),
            seen_session_ids: BTreeSet::new(),
            skipped_lines: 0,
            skipped_files: 0,
        })
    }

    fn record(&mut self, event: UsageEvent<'_>) {
        let UsageEvent {
            occurred_at,
            local_date,
            local_timestamp,
            timestamp,
            model,
            cwd,
            raw_usage,
            model_context_window,
            count_usage,
        } = event;
        let usage = token_usage(raw_usage);

        if count_usage {
            if self.windows.current.contains(local_date) {
                self.current.add_assign_saturating(usage);
                self.models
                    .entry(model.to_owned())
                    .or_default()
                    .add_assign_saturating(usage);
            } else if self.windows.previous_contains(local_timestamp) {
                self.previous.add_assign_saturating(usage);
            }

            if let Some(day) = self.daily.get_mut(&local_date) {
                day.add_assign_saturating(usage);
            }
        }

        let should_replace_latest = self
            .latest
            .as_ref()
            .map(|latest| occurred_at > latest.occurred_at)
            .unwrap_or(true);
        if should_replace_latest {
            self.latest = Some(LatestCandidate {
                occurred_at,
                usage: LatestContextUsage {
                    input_tokens: raw_usage.input_tokens,
                    cached_input_tokens: raw_usage.cached_input_tokens,
                    cache_write_input_tokens: raw_usage.cache_write_input_tokens,
                    output_tokens: raw_usage.output_tokens,
                    reasoning_output_tokens: raw_usage.reasoning_output_tokens,
                    total_tokens: raw_usage.context_total_tokens(),
                    model_context_window,
                    model: model.to_owned(),
                    timestamp,
                    cwd: cwd.map(Path::to_path_buf),
                },
            });
        }
    }

    fn record_unattributed_legacy(&mut self, event: &UsageEvent<'_>) {
        if event.count_usage && self.windows.current.contains(event.local_date) {
            self.unattributed_legacy
                .add_assign_saturating(token_usage(event.raw_usage));
        }
    }

    fn skip_line(&mut self) {
        self.skipped_lines = self.skipped_lines.saturating_add(1);
    }

    fn skip_file(&mut self) {
        self.skipped_files = self.skipped_files.saturating_add(1);
    }

    fn claim_file(&mut self, identity: FileIdentity) -> bool {
        self.seen_file_identities.insert(identity)
    }

    fn claim_session(&mut self, session_id: String) -> bool {
        self.seen_session_ids.insert(session_id)
    }

    fn finish(self) -> UsageReport {
        let mut model_distribution: Vec<_> = self
            .models
            .into_iter()
            .map(|(model, usage)| ModelUsage { model, usage })
            .collect();
        model_distribution.sort_by(|left, right| {
            right
                .usage
                .total_tokens()
                .cmp(&left.usage.total_tokens())
                .then_with(|| left.model.cmp(&right.model))
        });

        UsageReport {
            period: self.period,
            provider_filter: self.scope.provider_filter().map(str::to_owned),
            collected_at: self.collected_at,
            current_range: self.windows.current,
            previous_range: self.windows.previous,
            current: self.current,
            previous: self.previous,
            daily: self
                .daily
                .into_iter()
                .map(|(date, usage)| DailyUsage { date, usage })
                .collect(),
            model_distribution,
            latest_context: self.latest.map(|latest| latest.usage),
            unattributed_legacy: self.unattributed_legacy,
            skipped_lines: self.skipped_lines,
            skipped_files: self.skipped_files,
        }
    }
}

struct LatestCandidate {
    occurred_at: DateTime<FixedOffset>,
    usage: LatestContextUsage,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows { volume: u32, index: u64 },
    #[cfg(not(any(unix, windows)))]
    Canonical(PathBuf),
}

#[derive(Default)]
struct FileState {
    provider: Option<String>,
    provider_segment_started_at_unix_ms: Option<u64>,
    model: Option<String>,
    cwd: Option<PathBuf>,
    cumulative_usage: Option<RawTokenUsage>,
    saw_session_metadata: bool,
    duplicate_session: bool,
}

impl FileState {
    fn model(&self) -> &str {
        self.model.as_deref().unwrap_or(UNKNOWN_MODEL)
    }

    fn observe_cumulative_usage(&mut self, usage: Option<RawTokenUsage>) -> bool {
        match usage {
            Some(usage) => self.cumulative_usage.replace(usage) == Some(usage),
            None => {
                self.cumulative_usage = None;
                false
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RootScanStatus {
    Missing,
    Readable,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileScanStatus {
    Readable,
    Failed,
}

fn scan_root<Tz: TimeZone>(
    root: &Path,
    scope: &UsageScope,
    timezone: &Tz,
    accumulator: &mut UsageAccumulator,
) -> RootScanStatus {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if is_jsonl(root) {
                accumulator.skip_file();
            }
            return RootScanStatus::Missing;
        }
        Err(_) => {
            accumulator.skip_file();
            return RootScanStatus::Failed;
        }
    };
    if metadata.file_type().is_symlink() {
        accumulator.skip_file();
        return RootScanStatus::Failed;
    }
    if metadata.is_file() {
        if is_jsonl(root) {
            return match scan_file(root, scope, timezone, accumulator) {
                FileScanStatus::Readable => RootScanStatus::Readable,
                FileScanStatus::Failed => RootScanStatus::Failed,
            };
        }
        accumulator.skip_file();
        return RootScanStatus::Failed;
    }
    if !metadata.is_dir() {
        accumulator.skip_file();
        return RootScanStatus::Failed;
    }

    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => {
            accumulator.skip_file();
            return RootScanStatus::Failed;
        }
    };
    let mut pending_directories = vec![entries];
    let mut candidate_files = 0_u64;
    let mut readable_files = 0_u64;
    while let Some(entries) = pending_directories.pop() {
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    accumulator.skip_file();
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => {
                    accumulator.skip_file();
                    if is_jsonl(&path) {
                        candidate_files = candidate_files.saturating_add(1);
                    }
                    continue;
                }
            };

            if file_type.is_dir() {
                match fs::read_dir(&path) {
                    Ok(entries) => pending_directories.push(entries),
                    Err(_) => accumulator.skip_file(),
                }
            } else if file_type.is_file() && is_jsonl(&path) {
                candidate_files = candidate_files.saturating_add(1);
                if scan_file(&path, scope, timezone, accumulator) == FileScanStatus::Readable {
                    readable_files = readable_files.saturating_add(1);
                }
            } else if file_type.is_symlink() && is_jsonl(&path) {
                accumulator.skip_file();
            } else if is_jsonl(&path) {
                candidate_files = candidate_files.saturating_add(1);
                accumulator.skip_file();
            }
        }
    }
    if candidate_files > 0 && readable_files == 0 {
        RootScanStatus::Failed
    } else {
        RootScanStatus::Readable
    }
}

fn is_jsonl(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
}

fn scan_file<Tz: TimeZone>(
    path: &Path,
    scope: &UsageScope,
    timezone: &Tz,
    accumulator: &mut UsageAccumulator,
) -> FileScanStatus {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(_) => {
            accumulator.skip_file();
            return FileScanStatus::Failed;
        }
    };
    if let Some(identity) = file_identity(&file, path)
        && !accumulator.claim_file(identity)
    {
        return FileScanStatus::Readable;
    }
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    let mut state = FileState::default();

    loop {
        buffer.clear();
        match read_bounded_line(&mut reader, &mut buffer) {
            Ok(None) => break,
            Ok(Some(true)) => accumulator.skip_line(),
            Ok(Some(false)) => process_line(&buffer, scope, timezone, &mut state, accumulator),
            Err(_) => {
                accumulator.skip_file();
                return FileScanStatus::Failed;
            }
        }
    }
    FileScanStatus::Readable
}

fn file_identity(file: &File, path: &Path) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = file.metadata().ok()?;
        let _ = path;
        Some(FileIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        use std::mem::MaybeUninit;
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };

        let _ = path;
        let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
        // The handle remains valid for the call and Windows initializes the structure on success.
        if unsafe {
            GetFileInformationByHandle(file.as_raw_handle() as _, information.as_mut_ptr())
        } == 0
        {
            return None;
        }
        let information = unsafe { information.assume_init() };
        Some(FileIdentity::Windows {
            volume: information.dwVolumeSerialNumber,
            index: (u64::from(information.nFileIndexHigh) << 32)
                | u64::from(information.nFileIndexLow),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let metadata = file.metadata().ok()?;
        let _ = metadata;
        fs::canonicalize(path).ok().map(FileIdentity::Canonical)
    }
}

// Large response/tool events can be a single JSONL record. Keep refresh memory bounded and
// drain oversized records without retaining their bodies; usage records are only a few KiB.
fn read_bounded_line<R: BufRead>(reader: &mut R, output: &mut Vec<u8>) -> io::Result<Option<bool>> {
    let mut saw_bytes = false;
    let mut too_long = false;

    loop {
        let (consumed, reached_newline) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(saw_bytes.then_some(too_long));
            }

            saw_bytes = true;
            let consumed = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |position| position + 1);

            if !too_long {
                let remaining = MAX_JSONL_LINE_BYTES.saturating_sub(output.len());
                let retained = consumed.min(remaining);
                output.extend_from_slice(&available[..retained]);
                too_long = retained < consumed;
            }

            (consumed, available[consumed - 1] == b'\n')
        };
        reader.consume(consumed);

        if reached_newline {
            return Ok(Some(too_long));
        }
    }
}

fn process_line<Tz: TimeZone>(
    line: &[u8],
    scope: &UsageScope,
    timezone: &Tz,
    state: &mut FileState,
    accumulator: &mut UsageAccumulator,
) {
    if line.iter().all(u8::is_ascii_whitespace) {
        accumulator.skip_line();
        return;
    }

    let entry: LogEntry = match serde_json::from_slice(line) {
        Ok(entry) => entry,
        Err(_) => {
            accumulator.skip_line();
            return;
        }
    };

    match entry {
        LogEntry::SessionMeta { timestamp, payload } => {
            let SessionMetaPayload {
                id,
                session_id,
                model_provider,
                cwd,
            } = payload;
            if !state.saw_session_metadata {
                state.saw_session_metadata = true;
                if let Some(session_id) = nonempty(id).or_else(|| nonempty(session_id)) {
                    state.duplicate_session = !accumulator.claim_session(session_id);
                }
            }
            if state.provider.is_none()
                && let Some(provider) = nonempty(model_provider)
            {
                state.provider = Some(provider);
                state.provider_segment_started_at_unix_ms =
                    timestamp.as_deref().and_then(parse_unix_ms);
            }
            if state.cwd.is_none()
                && let Some(cwd) = cwd.filter(|cwd| !cwd.as_os_str().is_empty())
            {
                state.cwd = Some(cwd);
            }
        }
        LogEntry::TurnContext { payload } => {
            state.model = nonempty(payload.model);
        }
        LogEntry::EventMsg {
            timestamp,
            payload:
                EventMessagePayload::TokenCount {
                    info:
                        Some(TokenCountInfo {
                            last_token_usage: Some(raw_usage),
                            total_token_usage,
                            model_context_window,
                        }),
                },
        } => {
            if state.duplicate_session {
                return;
            }
            let Some(timestamp) = timestamp else {
                accumulator.skip_line();
                return;
            };
            let occurred_at = match DateTime::parse_from_rfc3339(&timestamp) {
                Ok(occurred_at) => occurred_at,
                Err(_) => {
                    accumulator.skip_line();
                    return;
                }
            };

            let duplicate = state.observe_cumulative_usage(total_token_usage);
            let context_only = !raw_usage.has_usage_components();
            if duplicate && !context_only {
                return;
            }
            let local_timestamp = occurred_at.with_timezone(timezone).naive_local();
            let local_date = local_timestamp.date();
            let event = UsageEvent {
                occurred_at,
                local_date,
                local_timestamp,
                timestamp,
                model: state.model(),
                cwd: state.cwd.as_deref(),
                raw_usage,
                model_context_window: model_context_window.unwrap_or_default(),
                count_usage: !context_only,
            };
            match scope.classify(
                state.provider.as_deref(),
                state.provider_segment_started_at_unix_ms,
            ) {
                UsageMatch::Included => accumulator.record(event),
                UsageMatch::UnattributedLegacy => accumulator.record_unattributed_legacy(&event),
                UsageMatch::Excluded => {}
            }
        }
        LogEntry::EventMsg {
            timestamp,
            payload: EventMessagePayload::ThreadSettingsApplied { thread_settings },
            ..
        } => {
            if let Some(provider) = nonempty(thread_settings.model_provider_id) {
                state.provider = Some(provider);
                state.provider_segment_started_at_unix_ms =
                    timestamp.as_deref().and_then(parse_unix_ms);
            }
            if let Some(model) = nonempty(thread_settings.model) {
                state.model = Some(model);
            }
            if let Some(cwd) = thread_settings
                .cwd
                .filter(|cwd| !cwd.as_os_str().is_empty())
            {
                state.cwd = Some(cwd);
            }
        }
        LogEntry::EventMsg { .. } | LogEntry::Other => {}
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UsageMatch {
    Included,
    Excluded,
    UnattributedLegacy,
}

impl UsageScope {
    fn classify(
        &self,
        provider: Option<&str>,
        provider_segment_started_at_unix_ms: Option<u64>,
    ) -> UsageMatch {
        if self.exact_provider_id.is_none() && self.legacy_provider_id.is_none() {
            return UsageMatch::Included;
        }
        if provider == self.exact_provider_id.as_deref() {
            return UsageMatch::Included;
        }
        if provider != self.legacy_provider_id.as_deref() {
            return UsageMatch::Excluded;
        }
        let Some(provider_segment_started_at_unix_ms) = provider_segment_started_at_unix_ms else {
            return UsageMatch::UnattributedLegacy;
        };
        if self.legacy_window_contains(provider_segment_started_at_unix_ms) {
            UsageMatch::Included
        } else if self.known_legacy_window_contains(provider_segment_started_at_unix_ms) {
            UsageMatch::Excluded
        } else {
            UsageMatch::UnattributedLegacy
        }
    }
}

fn parse_unix_ms(timestamp: &str) -> Option<u64> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()?
        .timestamp_millis()
        .try_into()
        .ok()
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

fn push_csv_field(output: &mut String, value: &str) {
    let formula_unsafe = value
        .chars()
        .next()
        .is_some_and(|character| matches!(character, '\t' | '\r' | '\n'))
        || value
            .trim_start_matches(char::is_whitespace)
            .chars()
            .next()
            .is_some_and(|character| matches!(character, '=' | '+' | '-' | '@'));
    if value
        .bytes()
        .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'))
    {
        output.push('"');
        if formula_unsafe {
            output.push('\'');
        }
        for character in value.chars() {
            if character == '"' {
                output.push('"');
            }
            output.push(character);
        }
        output.push('"');
    } else {
        if formula_unsafe {
            output.push('\'');
        }
        output.push_str(value);
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum LogEntry {
    #[serde(rename = "session_meta")]
    SessionMeta {
        timestamp: Option<String>,
        payload: SessionMetaPayload,
    },
    #[serde(rename = "turn_context")]
    TurnContext { payload: TurnContextPayload },
    #[serde(rename = "event_msg")]
    EventMsg {
        timestamp: Option<String>,
        payload: EventMessagePayload,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct SessionMetaPayload {
    id: Option<String>,
    session_id: Option<String>,
    model_provider: Option<String>,
    cwd: Option<PathBuf>,
}

#[derive(Deserialize)]
struct TurnContextPayload {
    model: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum EventMessagePayload {
    #[serde(rename = "token_count")]
    TokenCount { info: Option<TokenCountInfo> },
    #[serde(rename = "thread_settings_applied")]
    ThreadSettingsApplied {
        thread_settings: ThreadSettingsPayload,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct ThreadSettingsPayload {
    model_provider_id: Option<String>,
    model: Option<String>,
    cwd: Option<PathBuf>,
}

#[derive(Deserialize)]
struct TokenCountInfo {
    last_token_usage: Option<RawTokenUsage>,
    total_token_usage: Option<RawTokenUsage>,
    model_context_window: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
struct RawTokenUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

impl RawTokenUsage {
    fn has_usage_components(self) -> bool {
        self.input_tokens != 0
            || self.cached_input_tokens != 0
            || self.cache_write_input_tokens != 0
            || self.output_tokens != 0
            || self.reasoning_output_tokens != 0
    }

    fn context_total_tokens(self) -> u64 {
        if self.total_tokens == 0 {
            self.input_tokens.saturating_add(self.output_tokens)
        } else {
            self.total_tokens
        }
    }
}

fn token_usage(raw_usage: RawTokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: raw_usage.input_tokens,
        cached_input_tokens: raw_usage.cached_input_tokens,
        cache_write_input_tokens: raw_usage.cache_write_input_tokens,
        output_tokens: raw_usage.output_tokens,
        reasoning_output_tokens: raw_usage.reasoning_output_tokens,
        calls: 1,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::{FixedOffset, TimeZone};
    use serde_json::{Value, json};

    use super::*;

    fn test_now() -> DateTime<FixedOffset> {
        FixedOffset::east_opt(8 * 60 * 60)
            .unwrap()
            .with_ymd_and_hms(2026, 8, 26, 12, 0, 0)
            .single()
            .unwrap()
    }

    fn write_lines(path: &Path, lines: &[String]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut contents = lines.join("\n");
        contents.push('\n');
        fs::write(path, contents).unwrap();
    }

    fn metadata(provider: &str, cwd: &str) -> String {
        json!({
            "timestamp": "2026-08-13T00:00:00Z",
            "type": "session_meta",
            "payload": { "model_provider": provider, "cwd": cwd }
        })
        .to_string()
    }

    fn metadata_at(timestamp: &str, provider: &str, cwd: &str) -> String {
        json!({
            "timestamp": timestamp,
            "type": "session_meta",
            "payload": { "model_provider": provider, "cwd": cwd }
        })
        .to_string()
    }

    fn metadata_with_id(id: &str, provider: &str, cwd: &str) -> String {
        json!({
            "timestamp": "2026-08-13T00:00:00Z",
            "type": "session_meta",
            "payload": {
                "id": id,
                "session_id": id,
                "model_provider": provider,
                "cwd": cwd
            }
        })
        .to_string()
    }

    fn model(value: &str) -> String {
        json!({
            "timestamp": "2026-08-13T00:00:01Z",
            "type": "turn_context",
            "payload": { "model": value, "unrelated": "ignored" }
        })
        .to_string()
    }

    fn token(timestamp: &str, input: u64, cached: u64, output: u64, context_window: u64) -> String {
        json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": input,
                        "cached_input_tokens": cached,
                        "output_tokens": output,
                        "total_tokens": input.saturating_add(output)
                    },
                    "model_context_window": context_window
                }
            }
        })
        .to_string()
    }

    fn detailed_token(timestamp: &str) -> String {
        json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": 100,
                        "cached_input_tokens": 40,
                        "cache_write_input_tokens": 12,
                        "output_tokens": 30,
                        "reasoning_output_tokens": 9,
                        "total_tokens": 130
                    },
                    "model_context_window": 200000
                }
            }
        })
        .to_string()
    }

    fn token_snapshot(
        timestamp: &str,
        last_input: u64,
        last_output: u64,
        last_total: u64,
        cumulative_input: u64,
        cumulative_output: u64,
        context_window: u64,
    ) -> String {
        json!({
            "timestamp": timestamp,
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": cumulative_input,
                        "cached_input_tokens": 0,
                        "cache_write_input_tokens": 0,
                        "output_tokens": cumulative_output,
                        "reasoning_output_tokens": 0,
                        "total_tokens": cumulative_input.saturating_add(cumulative_output)
                    },
                    "last_token_usage": {
                        "input_tokens": last_input,
                        "cached_input_tokens": 0,
                        "cache_write_input_tokens": 0,
                        "output_tokens": last_output,
                        "reasoning_output_tokens": 0,
                        "total_tokens": last_total
                    },
                    "model_context_window": context_window
                }
            }
        })
        .to_string()
    }

    fn thread_settings(provider: &str, model: &str, cwd: &str) -> String {
        json!({
            "timestamp": "2026-08-26T03:00:00Z",
            "type": "event_msg",
            "payload": {
                "type": "thread_settings_applied",
                "thread_settings": {
                    "model_provider_id": provider,
                    "model": model,
                    "cwd": cwd,
                    "developer_instructions": "ignored"
                }
            }
        })
        .to_string()
    }

    fn collect_at(
        sessions: &Path,
        archived: &Path,
        period: UsagePeriod,
        provider: Option<&str>,
    ) -> UsageReport {
        collect_usage_at(sessions, archived, period, provider, test_now()).unwrap()
    }

    fn collect_scoped_at(
        sessions: &Path,
        archived: &Path,
        period: UsagePeriod,
        scope: &UsageScope,
    ) -> UsageReport {
        collect_usage_scoped_at(sessions, archived, period, scope, test_now()).unwrap()
    }

    fn unix_ms(timestamp: &str) -> u64 {
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .timestamp_millis()
            .try_into()
            .unwrap()
    }

    #[test]
    fn recursively_aggregates_current_previous_daily_and_models() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived_sessions");
        write_lines(
            &sessions.join("2026/08/session.jsonl"),
            &[
                metadata("relay-a", "/work/alpha"),
                model("old-model"),
                token("2026-08-19T08:00:00+08:00", 5, 1, 2, 100_000),
                model("model-a"),
                token("2026-08-20T00:00:00+08:00", 10, 3, 4, 200_000),
                model("model-b"),
                token("2026-08-26T23:00:00+08:00", 20, 7, 8, 300_000),
            ],
        );
        write_lines(
            &archived.join("ignored.txt"),
            &[
                metadata("relay-a", "/work/ignored"),
                token("2026-08-26T12:00:00+08:00", 999, 0, 0, 1),
            ],
        );

        let report = collect_at(&sessions, &archived, UsagePeriod::Last7Days, None);

        assert_eq!(
            report.current,
            TokenUsage {
                input_tokens: 30,
                cached_input_tokens: 10,
                output_tokens: 12,
                calls: 2,
                ..TokenUsage::default()
            }
        );
        assert_eq!(
            report.previous,
            TokenUsage {
                input_tokens: 5,
                cached_input_tokens: 1,
                output_tokens: 2,
                calls: 1,
                ..TokenUsage::default()
            }
        );
        assert_eq!(
            report.current_range,
            UsageDateRange {
                start: NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(),
                end_exclusive: NaiveDate::from_ymd_opt(2026, 8, 27).unwrap(),
            }
        );
        assert_eq!(
            report.previous_range.end_exclusive,
            report.current_range.start
        );
        assert_eq!(report.daily.len(), 14);
        assert_eq!(report.daily.first().unwrap().date.to_string(), "2026-08-13");
        assert_eq!(report.daily.last().unwrap().date.to_string(), "2026-08-26");
        assert_eq!(
            report
                .daily
                .iter()
                .find(|day| day.date.to_string() == "2026-08-19")
                .unwrap()
                .usage
                .input_tokens,
            5
        );
        assert_eq!(
            report
                .model_distribution
                .iter()
                .map(|entry| (entry.model.as_str(), entry.usage.input_tokens))
                .collect::<Vec<_>>(),
            vec![("model-b", 20), ("model-a", 10)]
        );

        let latest = report.latest_context.unwrap();
        assert_eq!(latest.model, "model-b");
        assert_eq!(latest.input_tokens, 20);
        assert_eq!(latest.model_context_window, 300_000);
        assert_eq!(latest.cwd, Some(PathBuf::from("/work/alpha")));
    }

    #[test]
    fn provider_filter_applies_to_totals_models_daily_and_latest_context() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived_sessions");
        write_lines(
            &sessions.join("relay-a.jsonl"),
            &[
                metadata("relay-a", "/work/a"),
                model("model-a"),
                token("2026-08-26T08:00:00Z", 11, 2, 3, 111_000),
            ],
        );
        write_lines(
            &archived.join("relay-b.jsonl"),
            &[
                metadata("relay-b", "/work/b"),
                model("model-b"),
                token("2026-08-26T09:00:00Z", 99, 50, 20, 222_000),
            ],
        );

        let report = collect_at(&sessions, &archived, UsagePeriod::Today, Some("relay-a"));

        assert_eq!(report.provider_filter.as_deref(), Some("relay-a"));
        assert_eq!(report.current.input_tokens, 11);
        assert_eq!(report.current.calls, 1);
        assert_eq!(report.model_distribution.len(), 1);
        assert_eq!(report.model_distribution[0].model, "model-a");
        assert_eq!(
            report.latest_context.as_ref().unwrap().cwd,
            Some(PathBuf::from("/work/a"))
        );
        assert_eq!(report.latest_context.unwrap().model_context_window, 111_000);
    }

    #[test]
    fn legacy_shared_usage_is_attributed_by_its_configuration_segment() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived_sessions");
        const LEGACY: &str = "codex_switch";
        const CASDAO: &str = "codex_switch_casdao";

        write_lines(
            &sessions.join("casdao.jsonl"),
            &[
                metadata_at("2026-08-26T10:36:00+08:00", LEGACY, "/work/casdao"),
                model("gpt-5.6-sol"),
                // The session began under casdao and can continue producing after a later
                // global switch. Its segment start, rather than this token timestamp, decides
                // which legacy window owns it.
                token("2026-08-26T10:40:00+08:00", 10, 0, 1, 100_000),
            ],
        );
        write_lines(
            &sessions.join("openai.jsonl"),
            &[
                metadata_at("2026-08-26T10:39:00+08:00", LEGACY, "/work/openai"),
                model("gpt-5.6-sol"),
                token("2026-08-26T10:40:00+08:00", 20, 0, 2, 100_000),
            ],
        );
        write_lines(
            &sessions.join("before-first-backup.jsonl"),
            &[
                metadata_at("2026-08-26T10:34:00+08:00", LEGACY, "/work/unknown"),
                model("gpt-5.6-sol"),
                token("2026-08-26T10:36:00+08:00", 3, 0, 1, 100_000),
            ],
        );
        write_lines(
            &sessions.join("new-provider.jsonl"),
            &[
                metadata_at("2026-08-26T09:00:00+08:00", CASDAO, "/work/exact"),
                model("gpt-5.6-sol"),
                token("2026-08-26T09:01:00+08:00", 7, 0, 1, 100_000),
            ],
        );

        let casdao_scope = UsageScope::profile(
            CASDAO,
            LEGACY,
            vec![LegacyUsageWindow::new(
                unix_ms("2026-08-26T10:35:48+08:00"),
                unix_ms("2026-08-26T10:38:14+08:00"),
            )],
            vec![
                LegacyUsageWindow::new(
                    unix_ms("2026-08-26T10:35:48+08:00"),
                    unix_ms("2026-08-26T10:38:14+08:00"),
                ),
                LegacyUsageWindow::new(
                    unix_ms("2026-08-26T10:38:14+08:00"),
                    unix_ms("2026-08-26T10:53:54+08:00"),
                ),
            ],
        );

        let report = collect_scoped_at(&sessions, &archived, UsagePeriod::Today, &casdao_scope);

        assert_eq!(report.current.input_tokens, 17);
        assert_eq!(report.current.calls, 2);
        assert_eq!(report.model_distribution.len(), 1);
        assert_eq!(report.model_distribution[0].usage.input_tokens, 17);
        assert_eq!(report.unattributed_legacy.input_tokens, 3);
        assert_eq!(report.unattributed_legacy.calls, 1);
    }

    #[test]
    fn model_override_uses_the_current_legacy_window_without_claiming_older_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived_sessions");
        const LEGACY: &str = "codex_switch";
        const OPENAI: &str = "codex_switch_openai";
        const CASDAO: &str = "codex_switch_casdao";
        let fallback_start = unix_ms("2026-08-26T10:00:00+08:00");

        write_lines(
            &sessions.join("terra-current.jsonl"),
            &[
                metadata_at("2026-08-26T10:01:00+08:00", LEGACY, "/work/current"),
                model("gpt-5.6-terra"),
                token("2026-08-26T10:10:00+08:00", 17, 9, 2, 400_000),
            ],
        );
        write_lines(
            &sessions.join("pre-change-session.jsonl"),
            &[
                metadata_at("2026-08-26T09:59:00+08:00", LEGACY, "/work/older"),
                model("gpt-5.6-sol"),
                // The token came later, but the session began before this configuration was
                // written. It must remain unassigned rather than be guessed as OpenAI.
                token("2026-08-26T10:10:00+08:00", 3, 1, 1, 400_000),
            ],
        );

        let current_window = LegacyUsageWindow::new(fallback_start, u64::MAX);
        let openai_scope =
            UsageScope::profile(OPENAI, LEGACY, vec![current_window], vec![current_window]);
        let openai = collect_scoped_at(&sessions, &archived, UsagePeriod::Today, &openai_scope);
        assert_eq!(openai.current.input_tokens, 17);
        assert_eq!(openai.current.cached_input_tokens, 9);
        assert_eq!(openai.current.calls, 1);
        assert_eq!(openai.model_distribution.len(), 1);
        assert_eq!(openai.model_distribution[0].model, "gpt-5.6-terra");
        assert_eq!(openai.unattributed_legacy.input_tokens, 3);
        assert_eq!(openai.unattributed_legacy.calls, 1);

        let casdao_scope = UsageScope::profile(CASDAO, LEGACY, Vec::new(), vec![current_window]);
        let casdao = collect_scoped_at(&sessions, &archived, UsagePeriod::Today, &casdao_scope);
        assert_eq!(casdao.current.calls, 0);
        assert!(casdao.model_distribution.is_empty());
        assert_eq!(casdao.unattributed_legacy.input_tokens, 3);
    }

    #[test]
    fn first_session_metadata_is_preserved_and_modern_settings_can_update_it() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived_sessions");
        write_lines(
            &sessions.join("settings.jsonl"),
            &[
                metadata("relay-a", "/work/a"),
                metadata("inherited-relay", "/work/inherited"),
                model("old-model"),
                token("2026-08-26T02:00:00Z", 10, 1, 2, 100),
                thread_settings("relay-c", "modern-model", "/work/c"),
                token("2026-08-26T04:00:00Z", 20, 2, 3, 200),
            ],
        );

        let first = collect_at(&sessions, &archived, UsagePeriod::Today, Some("relay-a"));
        assert_eq!(first.current.input_tokens, 10);
        assert_eq!(first.latest_context.unwrap().cwd, Some("/work/a".into()));

        let modern = collect_at(&sessions, &archived, UsagePeriod::Today, Some("relay-c"));
        assert_eq!(modern.current.input_tokens, 20);
        let latest = modern.latest_context.unwrap();
        assert_eq!(latest.model, "modern-model");
        assert_eq!(latest.cwd, Some("/work/c".into()));
    }

    #[test]
    fn malformed_lines_and_unreadable_jsonl_paths_are_counted_without_stopping() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let missing_jsonl = temp.path().join("missing.jsonl");
        let invalid_timestamp = token("not-a-timestamp", 20, 0, 1, 100);
        let unrelated: Value = json!({
            "timestamp": "2026-08-26T00:00:00Z",
            "type": "event_msg",
            "payload": {
                "type": "agent_message",
                "message": { "body": ["must", "be", "ignored"] },
                "info": "not token info"
            }
        });
        write_lines(
            &sessions.join("mixed.jsonl"),
            &[
                metadata("relay-a", "/work/a"),
                model("model-a"),
                "{ definitely not json".to_owned(),
                unrelated.to_string(),
                invalid_timestamp,
                token("2026-08-26T02:00:00Z", 7, 2, 1, 100),
            ],
        );

        let report = collect_at(&sessions, &missing_jsonl, UsagePeriod::Today, None);

        assert_eq!(report.current.input_tokens, 7);
        assert_eq!(report.current.calls, 1);
        assert_eq!(report.skipped_lines, 2);
        assert_eq!(report.skipped_files, 1);
    }

    #[test]
    fn missing_session_roots_are_a_successful_empty_scan() {
        let temp = tempfile::tempdir().unwrap();
        let report = collect_at(
            &temp.path().join("missing-sessions"),
            &temp.path().join("missing-archive"),
            UsagePeriod::Today,
            None,
        );

        assert_eq!(report.current, TokenUsage::default());
        assert_eq!(report.skipped_files, 0);
    }

    #[test]
    fn one_unreadable_root_still_returns_a_partial_report() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        let blocker = temp.path().join("not-a-directory");
        fs::write(&blocker, b"blocker").unwrap();

        let report = collect_at(
            &sessions,
            &blocker.join("archive"),
            UsagePeriod::Today,
            None,
        );

        assert_eq!(report.current, TokenUsage::default());
        assert_eq!(report.skipped_files, 1);
    }

    #[test]
    fn all_existing_session_roots_unreadable_returns_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let blocker = temp.path().join("not-a-directory");
        fs::write(&blocker, b"blocker").unwrap();

        let error = collect_usage_at(
            &blocker.join("sessions"),
            &blocker.join("archive"),
            UsagePeriod::Today,
            None,
            test_now(),
        )
        .unwrap_err();

        assert_eq!(error, UsageError::SessionRootsUnreadable);
    }

    #[cfg(unix)]
    #[test]
    fn readable_roots_with_only_unreadable_jsonl_candidates_return_an_error() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived");
        for path in [sessions.join("one.jsonl"), archived.join("two.jsonl")] {
            write_lines(&path, &[metadata("relay-a", "/work")]);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        }

        let error = collect_usage_at(&sessions, &archived, UsagePeriod::Today, None, test_now())
            .unwrap_err();

        assert_eq!(error, UsageError::SessionRootsUnreadable);
    }

    #[test]
    fn bounded_reader_drains_an_oversized_record_before_the_next_line() {
        let mut contents = vec![b'x'; MAX_JSONL_LINE_BYTES + 1];
        contents.extend_from_slice(b"\n{}\n");
        let mut reader = io::Cursor::new(contents);
        let mut buffer = Vec::new();

        assert_eq!(
            read_bounded_line(&mut reader, &mut buffer).unwrap(),
            Some(true)
        );
        assert_eq!(buffer.len(), MAX_JSONL_LINE_BYTES);

        buffer.clear();
        assert_eq!(
            read_bounded_line(&mut reader, &mut buffer).unwrap(),
            Some(false)
        );
        assert_eq!(buffer, b"{}\n");
    }

    #[cfg(unix)]
    #[test]
    fn jsonl_symlinks_are_not_followed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived_sessions");
        let external = temp.path().join("outside.jsonl");
        write_lines(
            &external,
            &[
                metadata("relay-a", "/private/outside"),
                model("outside-model"),
                token("2026-08-26T02:00:00Z", 999, 0, 0, 100),
            ],
        );
        fs::create_dir_all(&sessions).unwrap();
        symlink(&external, sessions.join("linked.jsonl")).unwrap();

        let report = collect_at(&sessions, &archived, UsagePeriod::Today, None);

        assert_eq!(report.current, TokenUsage::default());
        assert_eq!(report.skipped_files, 1);
    }

    #[test]
    fn copied_session_ids_are_counted_once_across_live_and_archived_roots() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived_sessions");
        let lines = [
            metadata_with_id("session-1", "relay-a", "/work/a"),
            model("model-a"),
            token("2026-08-26T02:00:00Z", 10, 2, 3, 100),
        ];
        write_lines(&sessions.join("live.jsonl"), &lines);
        write_lines(&archived.join("copy.jsonl"), &lines);

        let report = collect_at(&sessions, &archived, UsagePeriod::Today, None);

        assert_eq!(report.current.input_tokens, 10);
        assert_eq!(report.current.output_tokens, 3);
        assert_eq!(report.current.calls, 1);
    }

    #[cfg(unix)]
    #[test]
    fn hard_linked_jsonl_files_without_session_ids_are_counted_once() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived_sessions");
        let live = sessions.join("live.jsonl");
        write_lines(
            &live,
            &[
                metadata("relay-a", "/work/a"),
                model("model-a"),
                token("2026-08-26T02:00:00Z", 10, 2, 3, 100),
            ],
        );
        fs::create_dir_all(&archived).unwrap();
        fs::hard_link(&live, archived.join("linked.jsonl")).unwrap();

        let report = collect_at(&sessions, &archived, UsagePeriod::Today, None);

        assert_eq!(report.current.input_tokens, 10);
        assert_eq!(report.current.calls, 1);
    }

    #[test]
    fn local_midnight_boundaries_are_end_exclusive() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived_sessions");
        write_lines(
            &sessions.join("boundary.jsonl"),
            &[
                metadata("relay-a", "/work/a"),
                model("model-a"),
                token("2026-08-25T15:59:59Z", 1, 0, 0, 100),
                token("2026-08-25T16:00:00Z", 2, 0, 0, 100),
                token("2026-08-26T15:59:59Z", 3, 0, 0, 100),
                token("2026-08-26T16:00:00Z", 4, 0, 0, 100),
            ],
        );

        let report = collect_at(&sessions, &archived, UsagePeriod::Today, None);

        assert_eq!(report.previous.input_tokens, 0);
        assert_eq!(report.current.input_tokens, 5);
        assert_eq!(report.current.calls, 2);
        assert_eq!(report.latest_context.unwrap().input_tokens, 4);
    }

    #[test]
    fn all_periods_use_equal_length_previous_ranges() {
        let today = test_now().date_naive();
        for period in [
            UsagePeriod::Today,
            UsagePeriod::Last7Days,
            UsagePeriod::Last30Days,
        ] {
            let windows = UsageWindows::new(today, test_now().time(), period).unwrap();
            let current_days = windows
                .current
                .end_exclusive
                .signed_duration_since(windows.current.start)
                .num_days();
            let previous_days = windows
                .previous
                .end_exclusive
                .signed_duration_since(windows.previous.start)
                .num_days();

            assert_eq!(current_days, period.day_count());
            assert_eq!(previous_days, current_days);
            assert_eq!(windows.previous.end_exclusive, windows.current.start);
        }
    }

    #[test]
    fn previous_period_stops_at_the_same_local_time_of_day() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived_sessions");
        write_lines(
            &sessions.join("comparable.jsonl"),
            &[
                metadata("relay-a", "/work/a"),
                model("model-a"),
                token("2026-08-25T03:59:59Z", 10, 0, 1, 100),
                token("2026-08-25T04:00:00Z", 20, 0, 2, 100),
                token("2026-08-25T04:00:01Z", 40, 0, 4, 100),
                token("2026-08-26T03:00:00Z", 80, 0, 8, 100),
            ],
        );

        let report = collect_at(&sessions, &archived, UsagePeriod::Today, None);

        assert_eq!(report.previous.input_tokens, 30);
        assert_eq!(report.previous.output_tokens, 3);
        assert_eq!(report.previous.calls, 2);
        assert_eq!(report.current.input_tokens, 80);
    }

    #[test]
    fn aggregations_saturate_instead_of_overflowing() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived_sessions");
        write_lines(
            &sessions.join("large.jsonl"),
            &[
                metadata("relay-a", "/work/a"),
                model("large-model"),
                token(
                    "2026-08-26T01:00:00Z",
                    u64::MAX,
                    u64::MAX,
                    u64::MAX,
                    u64::MAX,
                ),
                token("2026-08-26T02:00:00Z", 1, 1, 1, 1),
            ],
        );

        let report = collect_at(&sessions, &archived, UsagePeriod::Today, None);

        assert_eq!(report.current.input_tokens, u64::MAX);
        assert_eq!(report.current.cached_input_tokens, u64::MAX);
        assert_eq!(report.current.output_tokens, u64::MAX);
        assert_eq!(report.current.total_tokens(), u64::MAX);
        assert_eq!(report.current.calls, 2);
        assert_eq!(report.model_distribution[0].usage.input_tokens, u64::MAX);
    }

    #[test]
    fn csv_escapes_commas_quotes_and_newlines() {
        let models = vec![
            ModelUsage {
                model: "plain".to_owned(),
                usage: TokenUsage {
                    input_tokens: 1,
                    cached_input_tokens: 2,
                    output_tokens: 3,
                    calls: 4,
                    ..TokenUsage::default()
                },
            },
            ModelUsage {
                model: "model, \"quoted\"\r\nnext".to_owned(),
                usage: TokenUsage {
                    input_tokens: 5,
                    cached_input_tokens: 6,
                    output_tokens: 7,
                    calls: 8,
                    ..TokenUsage::default()
                },
            },
        ];

        assert_eq!(
            model_distribution_csv(&models),
            concat!(
                "model,input_tokens,cached_input_tokens,cache_write_input_tokens,output_tokens,reasoning_output_tokens,calls\n",
                "plain,1,2,0,3,0,4\n",
                "\"model, \"\"quoted\"\"\r\nnext\",5,6,0,7,0,8\n",
            )
        );
    }

    #[test]
    fn csv_neutralizes_spreadsheet_formula_prefixes() {
        let models = vec![ModelUsage {
            model: "=HYPERLINK(\"https://example.test\",\"open\")".to_owned(),
            usage: TokenUsage {
                input_tokens: 1,
                calls: 1,
                ..TokenUsage::default()
            },
        }];

        let csv = model_distribution_csv(&models);

        assert!(csv.contains("\"'=HYPERLINK(\"\"https://example.test\"\",\"\"open\"\")\",1"));
    }

    #[test]
    fn report_csv_identifies_scope_and_includes_summary_daily_and_model_rows() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived");
        write_lines(
            &sessions.join("usage.jsonl"),
            &[
                metadata("relay-a", "/work"),
                model("gpt-test"),
                token("2026-08-26T03:00:00Z", 100, 40, 30, 200_000),
            ],
        );
        let report = collect_at(&sessions, &archived, UsagePeriod::Today, Some("relay-a"));

        let csv = report.model_distribution_csv();

        assert!(csv.starts_with("section,label,value,range_start,range_end_exclusive,collected_at,provider_filter,input_tokens"));
        assert!(csv.contains("metadata,period,today"));
        assert!(csv.contains("metadata,provider_filter,relay-a"));
        assert!(csv.contains("metadata,collected_at,2026-08-26T12:00:00+08:00"));
        assert!(csv.contains("summary,current,,2026-08-26,2026-08-27"));
        assert!(csv.contains("summary,previous_comparable,,2026-08-25,2026-08-26"));
        assert!(csv.contains("daily,2026-08-26,,2026-08-26,2026-08-27"));
        assert!(csv.contains("model,gpt-test,,2026-08-26,2026-08-27"));
    }

    #[test]
    fn preserves_cache_write_and_reasoning_subsets_without_double_counting() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived");
        write_lines(
            &sessions.join("usage.jsonl"),
            &[
                metadata("relay-a", "/work"),
                model("gpt-test"),
                detailed_token("2026-08-26T08:00:00+08:00"),
            ],
        );

        let report = collect_at(&sessions, &archived, UsagePeriod::Today, None);

        assert_eq!(report.current.input_tokens, 100);
        assert_eq!(report.current.cached_input_tokens, 40);
        assert_eq!(report.current.cache_write_input_tokens, 12);
        assert_eq!(report.current.output_tokens, 30);
        assert_eq!(report.current.reasoning_output_tokens, 9);
        assert_eq!(report.current.total_tokens(), 130);
        let latest = report.latest_context.unwrap();
        assert_eq!(latest.cache_write_input_tokens, 12);
        assert_eq!(latest.reasoning_output_tokens, 9);
    }

    #[test]
    fn cumulative_snapshots_are_deduplicated_and_compaction_only_updates_context() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        let archived = temp.path().join("archived");
        write_lines(
            &sessions.join("snapshots.jsonl"),
            &[
                metadata("relay-a", "/work"),
                model("gpt-test"),
                token_snapshot("2026-08-26T01:00:00Z", 100, 10, 110, 100, 10, 200_000),
                token_snapshot("2026-08-26T01:00:01Z", 100, 10, 110, 100, 10, 200_000),
                token_snapshot("2026-08-26T01:01:00Z", 50, 5, 55, 150, 15, 200_000),
                token_snapshot("2026-08-26T01:01:01Z", 0, 0, 12_597, 150, 15, 200_000),
            ],
        );

        let report = collect_at(&sessions, &archived, UsagePeriod::Today, None);

        assert_eq!(report.current.input_tokens, 150);
        assert_eq!(report.current.output_tokens, 15);
        assert_eq!(report.current.calls, 2);
        let latest = report.latest_context.unwrap();
        assert_eq!(latest.input_tokens, 0);
        assert_eq!(latest.output_tokens, 0);
        assert_eq!(latest.total_tokens, 12_597);
        assert_eq!(latest.timestamp, "2026-08-26T01:01:01Z");
    }
}
