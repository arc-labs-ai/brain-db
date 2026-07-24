//! Temporal-expressions extractor — native pattern tier.
//!
//! Parses memory text for temporal expressions (ISO dates, bare years,
//! `yesterday`/`today`/`tomorrow`, `last/this/next` periods, `N units
//! ago` / `in N units`, weekday names) and emits one
//! [`StatementMention`] per distinct resolved absolute time. Each
//! mention is a memory-anchored Event: the subject is the source memory
//! itself (`subject_is_memory = true`) and the object is the resolved
//! unix-nanos as a decimal string under `brain:occurred_at`.
//!
//! Relative dates anchor to the memory's event time when the client
//! supplied one (`Memory::occurred_at_unix_nanos`), else the server
//! write time (`created_at_unix_ms`), else the extraction wall clock.
//! That way a memory ingested today about a 2020 event resolves "last
//! week" against 2020, not the ingest date.

use brain_core::{ExtractorId, ExtractorKind, Memory};
use std::sync::LazyLock;
use time::{Date, Duration, Month, OffsetDateTime, Time, Weekday};

use crate::framework::extractor::{
    ExtractionContext, ExtractionFuture, ExtractionResult, Extractor,
};
use crate::framework::item::{ExtractedItem, StatementMention};

/// Stable id for the native temporal extractor. Reserved low integer —
/// pattern-tier native extractors occupy the small-id space.
const TEMPORAL_EXTRACTOR_ID: u32 = 4;

/// Schema-independent: this extractor's logic is hard-coded, so the
/// version tracks the parsing rules, not any uploaded schema.
const TEMPORAL_EXTRACTOR_VERSION: u32 = 1;

/// `StatementMention.kind` discriminant for Event. The mention's kind
/// space is 1-based (1=Fact, 2=Preference, 3=Event) — distinct from the
/// 0-based `brain_core::StatementKind` repr.
const EVENT_KIND: u8 = 3;

/// Predicate every temporal mention is filed under.
const OCCURRED_AT_PREDICATE: &str = "brain:occurred_at";

/// Fixed confidence for pattern-resolved temporal mentions — high
/// enough to persist, low enough that an LLM-tier Event with a richer
/// object can supersede it.
const TEMPORAL_CONFIDENCE: f32 = 0.6;

/// At most this many mentions per memory. A pathological "every day for
/// 30 days" memory shouldn't flood the graph; the cap bounds fan-out.
const MAX_EMISSIONS: usize = 8;

const NANOS_PER_DAY: u64 = 86_400_000_000_000;

// --- Regexes (compiled once, case-insensitive where it matters) -------

static RE_ISO: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b\d{4}-\d{2}-\d{2}\b").expect("invariant: ISO regex"));

static RE_YEAR: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b(?:19|20)\d{2}\b").expect("invariant: year regex"));

static RE_DEICTIC: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::RegexBuilder::new(r"\b(yesterday|today|tomorrow)\b")
        .case_insensitive(true)
        .build()
        .expect("invariant: deictic regex")
});

static RE_RELATIVE_PERIOD: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::RegexBuilder::new(r"\b(last|this|next)\s+(week|month|year)\b")
        .case_insensitive(true)
        .build()
        .expect("invariant: relative-period regex")
});

static RE_AGO: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::RegexBuilder::new(r"\b(\d{1,4})\s+(day|week|month|year)s?\s+ago\b")
        .case_insensitive(true)
        .build()
        .expect("invariant: ago regex")
});

static RE_IN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::RegexBuilder::new(r"\bin\s+(\d{1,4})\s+(day|week|month|year)s?\b")
        .case_insensitive(true)
        .build()
        .expect("invariant: in regex")
});

static RE_WEEKDAY: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::RegexBuilder::new(r"\b(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b")
        .case_insensitive(true)
        .build()
        .expect("invariant: weekday regex")
});

/// Month-name alternation (full names + 3-letter abbreviations, plus the
/// common four-letter `sept`). Full names precede abbreviations so the
/// longer form is preferred; the regex engine backtracks either way.
const MONTH_ALT: &str = r"(?:january|february|march|april|may|june|july|august|september|october|november|december|jan|feb|mar|apr|jun|jul|aug|sept|sep|oct|nov|dec)";

/// `D Month YYYY` / `D Month, YYYY` — day, then month name, then a
/// 4-digit year. Groups: 1=day, 2=month, 3=year.
static RE_DMY: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::RegexBuilder::new(&format!(
        r"\b(\d{{1,2}})\s+({MONTH_ALT})\.?,?\s+((?:19|20)\d{{2}})\b"
    ))
    .case_insensitive(true)
    .build()
    .expect("invariant: D-Month-Year regex")
});

/// `Month D YYYY` / `Month D, YYYY` — month name, then day (with optional
/// ordinal suffix), then a 4-digit year. Groups: 1=month, 2=day, 3=year.
static RE_MDY: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::RegexBuilder::new(&format!(
        r"\b({MONTH_ALT})\.?\s+(\d{{1,2}})(?:st|nd|rd|th)?,?\s+((?:19|20)\d{{2}})\b"
    ))
    .case_insensitive(true)
    .build()
    .expect("invariant: Month-D-Year regex")
});

/// `Month YYYY` — month name directly followed by a 4-digit year (no day).
/// Resolves to the 1st of that month. Groups: 1=month, 2=year.
static RE_MY: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::RegexBuilder::new(&format!(r"\b({MONTH_ALT})\.?,?\s+((?:19|20)\d{{2}})\b"))
        .case_insensitive(true)
        .build()
        .expect("invariant: Month-Year regex")
});

/// `Month D` — month name plus a day, no year. Resolves against the
/// anchor's year. Groups: 1=month, 2=day.
static RE_MD: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::RegexBuilder::new(&format!(
        r"\b({MONTH_ALT})\.?\s+(\d{{1,2}})(?:st|nd|rd|th)?\b"
    ))
    .case_insensitive(true)
    .build()
    .expect("invariant: Month-D regex")
});

/// Native temporal-expressions extractor.
#[derive(Debug, Default)]
pub struct TemporalExtractor;

impl TemporalExtractor {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Extractor for TemporalExtractor {
    fn id(&self) -> ExtractorId {
        ExtractorId::from(TEMPORAL_EXTRACTOR_ID)
    }

    fn kind(&self) -> ExtractorKind {
        ExtractorKind::Pattern
    }

    fn name(&self) -> &str {
        "brain:temporal_expressions"
    }

    fn extractor_version(&self) -> u32 {
        TEMPORAL_EXTRACTOR_VERSION
    }

    fn is_wired(&self) -> bool {
        true
    }

    fn run<'a>(&'a self, ctx: &'a ExtractionContext<'a>, mem: &'a Memory) -> ExtractionFuture<'a> {
        let started = ctx.now_unix_nanos;
        // Deterministic, fully synchronous body — wrapped in a ready
        // future to satisfy the boxed-future trait shape without ever
        // yielding to the executor.
        Box::pin(async move {
            let items = extract(mem, ctx.now_unix_nanos);
            ExtractionResult::success(items, started, ctx.now_unix_nanos)
        })
    }
}

// --- Core parsing -----------------------------------------------------

/// Resolve every supported temporal expression in `mem.text` to an
/// absolute unix-nanos timestamp, deduped and capped.
fn extract(mem: &Memory, now_unix_nanos: u64) -> Vec<ExtractedItem> {
    let text = match mem.text.as_deref() {
        Some(t) if !t.is_empty() => t,
        _ => return Vec::new(),
    };

    let anchor_nanos = mem
        .occurred_at_unix_nanos
        .or_else(|| (mem.created_at_unix_ms != 0).then(|| mem.created_at_unix_ms * 1_000_000))
        .unwrap_or(now_unix_nanos);

    let anchor = match OffsetDateTime::from_unix_timestamp_nanos(i128::from(anchor_nanos)) {
        Ok(dt) => dt,
        // anchor out of `time`'s representable range — bail rather than
        // emit garbage.
        Err(_) => return Vec::new(),
    };

    // Resolved timestamps in first-seen order; dedup is by value.
    let mut seen: Vec<u64> = Vec::new();
    let mut items: Vec<ExtractedItem> = Vec::new();

    // Track byte spans already consumed by an ISO or natural-language
    // date so the bare-year pass doesn't double-emit the year embedded in
    // "2020-01-15" or "8 May 2023".
    let mut consumed_spans: Vec<(usize, usize)> = Vec::new();

    // 1. ISO dates.
    for m in RE_ISO.find_iter(text) {
        consumed_spans.push((m.start(), m.end()));
        if let Some(ts) = parse_iso(m.as_str()) {
            push_unique(&mut items, &mut seen, ts);
        }
    }

    // 2. Natural-language dates. Run before the bare-year pass and record
    // each match's span so the year inside "8 May 2023" isn't re-emitted
    // as a Jan-1 bare year. Ordering within this group goes from most to
    // least specific (day+month+year, then month+year, then month+day);
    // the span-suppression check keeps a broader match from firing again
    // on the substring a narrower one already consumed.

    // 2a. "D Month YYYY" / "D Month, YYYY".
    for caps in RE_DMY.captures_iter(text) {
        let span = caps.get(0).expect("invariant: whole match");
        if overlaps(&consumed_spans, span.start(), span.end()) {
            continue;
        }
        if let (Some(day), Some(month)) = (caps[1].parse::<u8>().ok(), parse_month_name(&caps[2])) {
            if let Some(ts) = natural_date_nanos(&caps[3], month, day) {
                consumed_spans.push((span.start(), span.end()));
                push_unique(&mut items, &mut seen, ts);
            }
        }
    }

    // 2b. "Month D YYYY" / "Month D, YYYY".
    for caps in RE_MDY.captures_iter(text) {
        let span = caps.get(0).expect("invariant: whole match");
        if overlaps(&consumed_spans, span.start(), span.end()) {
            continue;
        }
        if let (Some(month), Some(day)) = (parse_month_name(&caps[1]), caps[2].parse::<u8>().ok()) {
            if let Some(ts) = natural_date_nanos(&caps[3], month, day) {
                consumed_spans.push((span.start(), span.end()));
                push_unique(&mut items, &mut seen, ts);
            }
        }
    }

    // 2c. "Month YYYY" → the 1st of that month.
    for caps in RE_MY.captures_iter(text) {
        let span = caps.get(0).expect("invariant: whole match");
        if overlaps(&consumed_spans, span.start(), span.end()) {
            continue;
        }
        if let Some(month) = parse_month_name(&caps[1]) {
            if let Some(ts) = natural_date_nanos(&caps[2], month, 1) {
                consumed_spans.push((span.start(), span.end()));
                push_unique(&mut items, &mut seen, ts);
            }
        }
    }

    // 2d. "Month D" with no year → resolve against the anchor's year.
    for caps in RE_MD.captures_iter(text) {
        let span = caps.get(0).expect("invariant: whole match");
        if overlaps(&consumed_spans, span.start(), span.end()) {
            continue;
        }
        if let (Some(month), Some(day)) = (parse_month_name(&caps[1]), caps[2].parse::<u8>().ok()) {
            let year = anchor.year();
            if let Ok(date) = Date::from_calendar_date(year, month, day) {
                if let Some(ts) = date_to_unix_nanos(date) {
                    consumed_spans.push((span.start(), span.end()));
                    push_unique(&mut items, &mut seen, ts);
                }
            }
        }
    }

    // 3. Bare 4-digit years (skip any overlapping a consumed span).
    for m in RE_YEAR.find_iter(text) {
        if overlaps(&consumed_spans, m.start(), m.end()) {
            continue;
        }
        if let Some(ts) = parse_year(m.as_str()) {
            push_unique(&mut items, &mut seen, ts);
        }
    }

    // 4. yesterday / today / tomorrow.
    for caps in RE_DEICTIC.captures_iter(text) {
        let word = caps[1].to_ascii_lowercase();
        let resolved = match word.as_str() {
            "yesterday" => anchor_nanos.checked_sub(NANOS_PER_DAY),
            "today" => Some(anchor_nanos),
            "tomorrow" => anchor_nanos.checked_add(NANOS_PER_DAY),
            _ => None,
        };
        if let Some(ts) = resolved {
            push_unique(&mut items, &mut seen, ts);
        }
    }

    // 5. last/this/next week|month|year.
    for caps in RE_RELATIVE_PERIOD.captures_iter(text) {
        let direction = caps[1].to_ascii_lowercase();
        let unit = caps[2].to_ascii_lowercase();
        let n: i64 = match direction.as_str() {
            "last" => -1,
            "this" => 0,
            "next" => 1,
            _ => continue,
        };
        if let Some(ts) = shift(anchor, &unit, n) {
            push_unique(&mut items, &mut seen, ts);
        }
    }

    // 6a. "N units ago".
    for caps in RE_AGO.captures_iter(text) {
        let Ok(n) = caps[1].parse::<i64>() else {
            continue;
        };
        let unit = caps[2].to_ascii_lowercase();
        if let Some(ts) = shift(anchor, &unit, -n) {
            push_unique(&mut items, &mut seen, ts);
        }
    }

    // 6b. "in N units".
    for caps in RE_IN.captures_iter(text) {
        let Ok(n) = caps[1].parse::<i64>() else {
            continue;
        };
        let unit = caps[2].to_ascii_lowercase();
        if let Some(ts) = shift(anchor, &unit, n) {
            push_unique(&mut items, &mut seen, ts);
        }
    }

    // 7. Weekday names → nearest prior such weekday relative to anchor.
    for caps in RE_WEEKDAY.captures_iter(text) {
        if let Some(ts) = prior_weekday(anchor, &caps[1].to_ascii_lowercase()) {
            push_unique(&mut items, &mut seen, ts);
        }
    }

    items.truncate(MAX_EMISSIONS);
    items
}

/// Append a mention for `ts` unless that timestamp was already emitted.
/// No-op once the per-memory cap is hit (truncation also enforces the
/// cap, but stopping early avoids needless allocation).
fn push_unique(items: &mut Vec<ExtractedItem>, seen: &mut Vec<u64>, ts: u64) {
    if items.len() >= MAX_EMISSIONS {
        return;
    }
    if seen.contains(&ts) {
        return;
    }
    seen.push(ts);
    items.push(mention(ts));
}

/// Build the canonical memory-anchored Event mention for a resolved
/// timestamp.
fn mention(ts: u64) -> ExtractedItem {
    ExtractedItem::StatementMention(StatementMention {
        kind: EVENT_KIND,
        subject_text: None,
        predicate_qname: OCCURRED_AT_PREDICATE.to_string(),
        object_text: Some(ts.to_string()),
        confidence: TEMPORAL_CONFIDENCE,
        extractor_id: TEMPORAL_EXTRACTOR_ID,
        extractor_version: TEMPORAL_EXTRACTOR_VERSION,
        is_stateful: false,
        subject_is_memory: true,
        // The object IS the time; carry it as the resolved event time too so
        // the apply pass plumbs `event_at` directly.
        object_is_entity: false,
        event_at_unix_nanos: Some(ts),
        // Subject is the source memory, not the space.
        subject_is_self: false,
        // A temporal occurrence is an assertion, never a retraction.
        retract: false,
    })
}

// --- Resolution helpers -----------------------------------------------

/// Civil date at 00:00:00 UTC → unix-nanos. Returns `None` when the
/// resulting instant falls outside the `u64` nanos range (pre-1970 or
/// far future).
fn date_to_unix_nanos(date: Date) -> Option<u64> {
    let dt = OffsetDateTime::new_utc(date, Time::MIDNIGHT);
    u64::try_from(dt.unix_timestamp_nanos()).ok()
}

/// Parse a `YYYY-MM-DD` string to the unix-nanos of that civil date at
/// midnight UTC. `None` on any out-of-range component.
fn parse_iso(s: &str) -> Option<u64> {
    let mut parts = s.split('-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u8 = parts.next()?.parse().ok()?;
    let d: u8 = parts.next()?.parse().ok()?;
    let month = Month::try_from(m).ok()?;
    let date = Date::from_calendar_date(y, month, d).ok()?;
    date_to_unix_nanos(date)
}

/// Parse a bare 4-digit year to Jan 1 of that year at midnight UTC.
fn parse_year(s: &str) -> Option<u64> {
    let y: i32 = s.parse().ok()?;
    let date = Date::from_calendar_date(y, Month::January, 1).ok()?;
    date_to_unix_nanos(date)
}

/// True when the half-open byte range `[s, e)` overlaps any span already
/// consumed by an earlier date pass.
fn overlaps(spans: &[(usize, usize)], s: usize, e: usize) -> bool {
    spans.iter().any(|&(a, b)| s < b && e > a)
}

/// Resolve a full month name or 3-letter abbreviation (case-insensitive,
/// with an optional trailing `.`) to a [`Month`]. `sept` is accepted as a
/// common four-letter form for September.
fn parse_month_name(s: &str) -> Option<Month> {
    let s = s.trim().trim_end_matches('.').to_ascii_lowercase();
    Some(match s.as_str() {
        "january" | "jan" => Month::January,
        "february" | "feb" => Month::February,
        "march" | "mar" => Month::March,
        "april" | "apr" => Month::April,
        "may" => Month::May,
        "june" | "jun" => Month::June,
        "july" | "jul" => Month::July,
        "august" | "aug" => Month::August,
        "september" | "sep" | "sept" => Month::September,
        "october" | "oct" => Month::October,
        "november" | "nov" => Month::November,
        "december" | "dec" => Month::December,
        _ => return None,
    })
}

/// Resolve a `(year-string, month, day)` calendar date to unix-nanos at
/// midnight UTC. `None` on an out-of-range component (e.g. Feb 30).
fn natural_date_nanos(year_s: &str, month: Month, day: u8) -> Option<u64> {
    let y: i32 = year_s.parse().ok()?;
    let date = Date::from_calendar_date(y, month, day).ok()?;
    date_to_unix_nanos(date)
}

/// True when `surface`, in its entirety, is a calendar date that pins a
/// specific year — an ISO date, `D Month YYYY`, `Month D, YYYY`,
/// `Month YYYY`, or a bare 4-digit year. This is the single authority on
/// "is this whole string a date", shared with the entity-resolution guard
/// so both agree on what counts as one; it reuses the same recognizers the
/// extraction pass above runs on free text.
///
/// **Whole-surface anchored**: a recognizer's match must cover the trimmed
/// input end to end. A year merely *embedded* in a name ("Room 2026",
/// "Project Apollo 1969") therefore is not a date and stays eligible to be
/// an entity.
///
/// Deliberately excludes the year-less `Month D` form and bare month /
/// weekday names: "May" and "May 8" can plausibly name a person or product,
/// and the guard's contract is to reject only unambiguous dates.
#[must_use]
pub(crate) fn is_full_date_surface(surface: &str) -> bool {
    let s = surface.trim();
    if s.is_empty() {
        return false;
    }
    // Most specific first; `any` short-circuits, so "2020-01-15" is claimed
    // by the ISO recognizer rather than tested against the bare-year one.
    [&*RE_ISO, &*RE_DMY, &*RE_MDY, &*RE_MY, &*RE_YEAR]
        .into_iter()
        .any(|re| matches_whole(re, s))
}

/// True when `re`'s leftmost match spans all of `s`. The date regexes are
/// `\b`-delimited, so a leftmost match that starts at 0 and ends at the
/// input length means the entire surface parsed as that form.
fn matches_whole(re: &regex::Regex, s: &str) -> bool {
    re.find(s)
        .is_some_and(|m| m.start() == 0 && m.end() == s.len())
}

/// Parse an event-date string to unix-nanos at midnight UTC. Accepts an ISO
/// `YYYY-MM-DD` date or a bare `YYYY` year. Shared with the LLM tier, which
/// asks the model for an ISO date (models are reliable at dates, not at raw
/// epoch-nanos) and resolves it here. `None` on any unparseable input.
pub(crate) fn parse_event_date(s: &str) -> Option<u64> {
    let s = s.trim();
    parse_iso(s).or_else(|| parse_year(s))
}

/// Shift `anchor` by `n` units (`n` may be negative). `week`/`day` use
/// fixed durations; `month`/`year` use calendar arithmetic so month
/// length and leap years are honoured.
fn shift(anchor: OffsetDateTime, unit: &str, n: i64) -> Option<u64> {
    let shifted = match unit {
        "day" => anchor.checked_add(Duration::days(n))?,
        "week" => anchor.checked_add(Duration::weeks(n))?,
        "month" => shift_months(anchor, n)?,
        "year" => shift_months(anchor, n.checked_mul(12)?)?,
        _ => return None,
    };
    u64::try_from(shifted.unix_timestamp_nanos()).ok()
}

/// Add `n` calendar months to `dt`, clamping the day-of-month to the
/// target month's length (Jan 31 + 1 month → Feb 28/29). Time-of-day is
/// preserved.
fn shift_months(dt: OffsetDateTime, n: i64) -> Option<OffsetDateTime> {
    let date = dt.date();
    // Zero-based month index, 0 = January of year `date.year()`.
    let base = i64::from(date.year())
        .checked_mul(12)?
        .checked_add(i64::from(date.month() as u8) - 1)?;
    let total = base.checked_add(n)?;
    let new_year = i32::try_from(total.div_euclid(12)).ok()?;
    let new_month_idx = total.rem_euclid(12) as u8; // 0..=11
    let new_month = Month::try_from(new_month_idx + 1).ok()?;
    let max_day = days_in_month(new_year, new_month);
    let day = date.day().min(max_day);
    let new_date = Date::from_calendar_date(new_year, new_month, day).ok()?;
    Some(dt.replace_date(new_date))
}

/// Days in a given month, leap-year aware.
fn days_in_month(year: i32, month: Month) -> u8 {
    match month {
        Month::January
        | Month::March
        | Month::May
        | Month::July
        | Month::August
        | Month::October
        | Month::December => 31,
        Month::April | Month::June | Month::September | Month::November => 30,
        Month::February => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Resolve a weekday name to the nearest *prior* such weekday relative
/// to `anchor`'s date, at midnight UTC. If the anchor itself falls on
/// that weekday, the prior occurrence (7 days earlier) is used so the
/// result is unambiguously in the past.
fn prior_weekday(anchor: OffsetDateTime, name: &str) -> Option<u64> {
    let target = weekday_from_name(name)?;
    let anchor_date = anchor.date();
    // `number_from_monday`: Monday = 1 .. Sunday = 7.
    let cur = i64::from(anchor_date.weekday().number_from_monday());
    let tgt = i64::from(target.number_from_monday());
    // Days back to the most recent prior occurrence (1..=7).
    let mut back = (cur - tgt).rem_euclid(7);
    if back == 0 {
        back = 7;
    }
    let date = anchor_date.checked_sub(Duration::days(back))?;
    date_to_unix_nanos(date)
}

fn weekday_from_name(name: &str) -> Option<Weekday> {
    Some(match name {
        "monday" => Weekday::Monday,
        "tuesday" => Weekday::Tuesday,
        "wednesday" => Weekday::Wednesday,
        "thursday" => Weekday::Thursday,
        "friday" => Weekday::Friday,
        "saturday" => Weekday::Saturday,
        "sunday" => Weekday::Sunday,
        _ => return None,
    })
}

// --- Tests ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::registry::ExtractorRegistry;
    use brain_core::{MemoryId, MemoryKind, Salience, SessionId, SpaceId};

    /// Build the unix-nanos of a civil date at midnight UTC. Used in
    /// place of `time::macros::datetime!` so the tests don't require the
    /// `time` `macros` feature.
    fn utc_midnight(year: i32, month: u8, day: u8) -> u64 {
        let date = Date::from_calendar_date(year, Month::try_from(month).unwrap(), day).unwrap();
        let dt = OffsetDateTime::new_utc(date, Time::MIDNIGHT);
        u64::try_from(dt.unix_timestamp_nanos()).unwrap()
    }

    fn ctx(reg: &ExtractorRegistry) -> ExtractionContext<'_> {
        ExtractionContext {
            declared_entity_types: None,
            candidate_predicates: None,
            declared_kinds: None,
            entity_type_labels: None,
            schema_version: 1,
            // A fixed, recognisable "now" so tests that fall through to
            // it are deterministic: 2024-06-01T00:00:00Z.
            now_unix_nanos: utc_midnight(2024, 6, 1),
            registry: reg,
            prior_tier_items: None,
            extractor_context: None,
        }
    }

    /// Memory with an explicit `occurred_at` anchor and no
    /// `created_at`.
    fn memory_at(text: &str, occurred_at_unix_nanos: u64) -> Memory {
        Memory {
            id: MemoryId::pack(0, 1, 0),
            space: SpaceId::new(),
            session_id: SessionId(0),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            text: Some(text.to_string()),
            created_at_unix_ms: 0,
            last_accessed_at_unix_ms: 0,
            occurred_at_unix_nanos: Some(occurred_at_unix_nanos),
        }
    }

    fn anchor_2024_06_01() -> u64 {
        utc_midnight(2024, 6, 1)
    }

    fn run(text: &str, occurred_at: u64) -> Vec<ExtractedItem> {
        let reg = ExtractorRegistry::new();
        let ext = TemporalExtractor::new();
        let mem = memory_at(text, occurred_at);
        futures_lite::future::block_on(ext.run(&ctx(&reg), &mem)).items
    }

    fn object_nanos(item: &ExtractedItem) -> u64 {
        let ExtractedItem::StatementMention(m) = item else {
            panic!("expected StatementMention");
        };
        m.object_text.as_deref().unwrap().parse().unwrap()
    }

    #[test]
    fn temporal_extractor_reports_stable_identity_metadata() {
        let ext = TemporalExtractor::new();
        assert_eq!(ext.id(), ExtractorId::from(4));
        assert_eq!(ext.kind(), ExtractorKind::Pattern);
        assert_eq!(ext.name(), "brain:temporal_expressions");
        assert_eq!(ext.extractor_version(), 1);
        assert!(ext.is_wired());
    }

    #[test]
    fn iso_date_resolves_to_midnight_utc() {
        let items = run("we shipped on 2020-01-15 finally", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), utc_midnight(2020, 1, 15));
    }

    #[test]
    fn n_years_ago_uses_calendar_year() {
        let items = run("that was 4 years ago", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
        let dt =
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(object_nanos(&items[0]))).unwrap();
        assert_eq!(dt.year(), 2020);
        assert_eq!(dt.month(), Month::June);
        assert_eq!(dt.day(), 1);
    }

    #[test]
    fn yesterday_is_anchor_minus_one_day() {
        let anchor = anchor_2024_06_01();
        let items = run("I saw it yesterday", anchor);
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), anchor - 86_400_000_000_000);
    }

    #[test]
    fn today_and_tomorrow() {
        let anchor = anchor_2024_06_01();
        let items = run("today and tomorrow", anchor);
        let resolved: Vec<u64> = items.iter().map(object_nanos).collect();
        assert!(resolved.contains(&anchor));
        assert!(resolved.contains(&(anchor + 86_400_000_000_000)));
    }

    #[test]
    fn bare_year_resolves_to_jan_first() {
        let items = run("back in 2022 it began", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), utc_midnight(2022, 1, 1));
    }

    #[test]
    fn iso_year_not_double_emitted_as_bare_year() {
        // "2020-01-15" contains "2020"; only the ISO date must emit.
        let items = run("on 2020-01-15", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), utc_midnight(2020, 1, 15));
    }

    #[test]
    fn occurred_at_takes_precedence_over_created_at() {
        // occurred_at = 2024 (4-years-ago → 2020); created_at = 2010
        // (would give 2006). Resolution must follow occurred_at.
        let reg = ExtractorRegistry::new();
        let ext = TemporalExtractor::new();
        let mem = Memory {
            id: MemoryId::pack(0, 1, 0),
            space: SpaceId::new(),
            session_id: SessionId(0),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            text: Some("4 years ago".to_string()),
            created_at_unix_ms: utc_midnight(2010, 6, 1) / 1_000_000,
            last_accessed_at_unix_ms: 0,
            occurred_at_unix_nanos: Some(anchor_2024_06_01()),
        };
        let items = futures_lite::future::block_on(ext.run(&ctx(&reg), &mem)).items;
        assert_eq!(items.len(), 1);
        let dt =
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(object_nanos(&items[0]))).unwrap();
        assert_eq!(dt.year(), 2020);
    }

    #[test]
    fn created_at_used_when_occurred_at_absent() {
        let reg = ExtractorRegistry::new();
        let ext = TemporalExtractor::new();
        let mem = Memory {
            id: MemoryId::pack(0, 1, 0),
            space: SpaceId::new(),
            session_id: SessionId(0),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            text: Some("4 years ago".to_string()),
            created_at_unix_ms: utc_midnight(2010, 6, 1) / 1_000_000,
            last_accessed_at_unix_ms: 0,
            occurred_at_unix_nanos: None,
        };
        let items = futures_lite::future::block_on(ext.run(&ctx(&reg), &mem)).items;
        assert_eq!(items.len(), 1);
        let dt =
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(object_nanos(&items[0]))).unwrap();
        assert_eq!(dt.year(), 2006);
    }

    #[test]
    fn no_temporal_expression_yields_no_items() {
        let items = run("just a plain sentence with no dates", anchor_2024_06_01());
        assert!(items.is_empty());
    }

    #[test]
    fn empty_text_yields_no_items() {
        let items = run("", anchor_2024_06_01());
        assert!(items.is_empty());
    }

    #[test]
    fn all_mentions_are_memory_anchored_events() {
        let items = run(
            "on 2020-01-15 and 4 years ago and yesterday",
            anchor_2024_06_01(),
        );
        assert!(!items.is_empty());
        for item in &items {
            let ExtractedItem::StatementMention(m) = item else {
                panic!("expected StatementMention");
            };
            assert!(m.subject_is_memory);
            assert_eq!(m.predicate_qname, "brain:occurred_at");
            assert_eq!(m.kind, 3);
            assert_eq!(m.extractor_id, 4);
            assert_eq!(m.extractor_version, 1);
            assert!((m.confidence - 0.6).abs() < 1e-6);
            assert!(m.subject_text.is_none());
            assert!(!m.is_stateful);
        }
    }

    #[test]
    fn duplicate_timestamps_deduped() {
        // "today" and the ISO of the anchor date both resolve to the
        // same midnight instant → one mention.
        let items = run("today on 2024-06-01", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn emissions_capped_at_eight() {
        // Nine distinct ISO dates → cap to 8.
        let text = "2001-01-01 2002-01-01 2003-01-01 2004-01-01 2005-01-01 \
                    2006-01-01 2007-01-01 2008-01-01 2009-01-01";
        let items = run(text, anchor_2024_06_01());
        assert_eq!(items.len(), 8);
    }

    #[test]
    fn last_this_next_periods() {
        let anchor = anchor_2024_06_01();
        // last week.
        let items = run("last week", anchor);
        assert_eq!(object_nanos(&items[0]), anchor - 7 * 86_400_000_000_000);
        // this year resolves to the anchor instant.
        let items = run("this year", anchor);
        assert_eq!(object_nanos(&items[0]), anchor);
        // next month: 2024-06-01 → 2024-07-01.
        let items = run("next month", anchor);
        assert_eq!(object_nanos(&items[0]), utc_midnight(2024, 7, 1));
    }

    #[test]
    fn in_n_units() {
        let anchor = anchor_2024_06_01();
        let items = run("in 3 days", anchor);
        assert_eq!(object_nanos(&items[0]), anchor + 3 * 86_400_000_000_000);
    }

    #[test]
    fn month_shift_clamps_day_of_month() {
        // 2024-01-31 + 1 month → Feb (2024 is a leap year) → Feb 29.
        let anchor = utc_midnight(2024, 1, 31);
        let items = run("next month", anchor);
        let dt =
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(object_nanos(&items[0]))).unwrap();
        assert_eq!(dt.month(), Month::February);
        assert_eq!(dt.day(), 29);
    }

    #[test]
    fn weekday_resolves_to_prior_occurrence() {
        // Anchor 2024-06-01 is a Saturday. "Monday" → most recent prior
        // Monday = 2024-05-27.
        let items = run("we met on Monday", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), utc_midnight(2024, 5, 27));
    }

    #[test]
    fn weekday_on_same_weekday_uses_prior_week() {
        // Anchor 2024-06-01 is a Saturday. "Saturday" → prior Saturday
        // = 2024-05-25.
        let items = run("Saturday", anchor_2024_06_01());
        assert_eq!(object_nanos(&items[0]), utc_midnight(2024, 5, 25));
    }

    #[test]
    fn case_insensitive_matching() {
        let anchor = anchor_2024_06_01();
        let items = run("YESTERDAY", anchor);
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), anchor - 86_400_000_000_000);
    }

    #[test]
    fn natural_day_month_year_resolves() {
        // "8 May, 2023" must resolve to 2023-05-08, NOT the bare-year
        // fallback of 2023-01-01.
        let items = run("we met on 8 May, 2023 downtown", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), utc_midnight(2023, 5, 8));
    }

    #[test]
    fn natural_day_month_year_no_comma() {
        let items = run("we met on 8 May 2023 downtown", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), utc_midnight(2023, 5, 8));
    }

    #[test]
    fn natural_month_day_year_resolves() {
        let items = run("shipped May 8, 2023 finally", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), utc_midnight(2023, 5, 8));
    }

    #[test]
    fn natural_month_day_year_no_comma() {
        let items = run("shipped May 8 2023 finally", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), utc_midnight(2023, 5, 8));
    }

    #[test]
    fn two_natural_dates_no_spurious_years() {
        // Both dates resolve; neither embedded year leaks a Jan-1 mention.
        let items = run(
            "went on 8 May 2023 and again 9 June 2023",
            anchor_2024_06_01(),
        );
        let resolved: Vec<u64> = items.iter().map(object_nanos).collect();
        assert_eq!(resolved.len(), 2);
        assert!(resolved.contains(&utc_midnight(2023, 5, 8)));
        assert!(resolved.contains(&utc_midnight(2023, 6, 9)));
        // No Jan-1-2023 from the embedded years.
        assert!(!resolved.contains(&utc_midnight(2023, 1, 1)));
    }

    #[test]
    fn month_year_resolves_to_first_of_month() {
        let items = run("sometime in May 2023 it happened", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), utc_midnight(2023, 5, 1));
    }

    #[test]
    fn natural_date_year_not_double_emitted() {
        // "8 May 2023" must yield exactly one mention — the bare-year pass
        // must not also emit "2023".
        let items = run("8 May 2023", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), utc_midnight(2023, 5, 8));
    }

    #[test]
    fn three_letter_month_abbreviation() {
        let items = run("logged 8 Jun 2023", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), utc_midnight(2023, 6, 8));
    }

    #[test]
    fn full_date_surface_recognizes_year_bearing_forms() {
        for s in [
            "2020-01-15",
            "January 2026",
            "jan 2026",
            "Sept. 2026",
            "January 5, 2026",
            "January 5 2026",
            "January 5th, 2026",
            "5 January 2026",
            "8 May, 2023",
            "2026",
            "  January 2026  ",
        ] {
            assert!(is_full_date_surface(s), "{s} should be a full date");
        }
    }

    #[test]
    fn full_date_surface_rejects_partial_and_embedded_forms() {
        for s in [
            "",
            "   ",
            // No year → deliberately not a full date; could name a referent.
            "January",
            "May 8",
            "Friday",
            // A year embedded in a longer surface is part of a name.
            "Room 2026",
            "Project 2026",
            "Q1 2026",
            "2026 Roadmap",
            "January 2026 retro",
            "Diego",
            "Stripe",
            // Three digits / five digits are not years.
            "202",
            "20260",
        ] {
            assert!(!is_full_date_surface(s), "{s} should not be a full date");
        }
    }

    #[test]
    fn month_day_without_year_uses_anchor_year() {
        // Anchor is 2024 → "May 8" resolves to 2024-05-08.
        let items = run("due May 8", anchor_2024_06_01());
        assert_eq!(items.len(), 1);
        assert_eq!(object_nanos(&items[0]), utc_midnight(2024, 5, 8));
    }
}
