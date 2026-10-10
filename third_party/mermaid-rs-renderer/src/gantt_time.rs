//! Calendar arithmetic for Gantt charts: `dateFormat` parsing, task durations,
//! `axisFormat` formatting and axis tick placement.
//!
//! An instant is an `f64` count of days since 1970-01-01T00:00 in a time-zone
//! free proleptic Gregorian calendar: the integer part is the day number, the
//! fraction is the time of day. A chart never needs a UTC offset because every
//! instant in it is read and printed with the same (absent) offset.

const MS_PER_DAY: f64 = 86_400_000.0;
const MS_PER_DAY_I64: i64 = 86_400_000;
/// Tolerance (in days, about 86 microseconds) used when comparing tick positions.
const EPS: f64 = 1.0e-9;
/// Upper bound on a single duration amount (`1000000y`); keeps calendar maths far from overflow.
const MAX_DURATION_AMOUNT: f64 = 1.0e6;
/// A tick candidate that would place more ticks than this is rejected.
const MAX_TICKS: usize = 200;

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

pub(crate) fn is_leap_year(year: i64) -> bool {
    year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0)
}

pub(crate) fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap_year(year) => 29,
        _ => 28,
    }
}

/// Day number (days since 1970-01-01) of a civil date.
pub(crate) fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let d = i64::from(day);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil `(year, month, day)` of a day number (days since 1970-01-01).
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + i64::from(m <= 2), m as u32, d as u32)
}

#[derive(Debug, Clone, PartialEq)]
enum DateToken {
    Year4,
    Year2,
    MonthName { long: bool },
    Month,
    Day,
    Hour,
    Minute,
    Second,
    UnixSeconds,
    UnixMillis,
    Literal(char),
}

/// A parsed Mermaid `dateFormat` pattern (the moment.js subset Mermaid documents:
/// `YYYY YY MMMM MMM MM M DD D HH H mm m ss s X x` plus literal text, `[..]` escapes included).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DateFormat {
    tokens: Vec<DateToken>,
}

impl Default for DateFormat {
    /// Mermaid's default `dateFormat`: `YYYY-MM-DD`.
    fn default() -> Self {
        Self::new("YYYY-MM-DD")
    }
}

impl DateFormat {
    pub(crate) fn new(pattern: &str) -> Self {
        let chars: Vec<char> = pattern.chars().collect();
        let mut tokens = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            let run = chars[i..].iter().take_while(|&&x| x == c).count();
            let (token, len) = match c {
                'Y' if run >= 4 => (DateToken::Year4, 4),
                'Y' if run >= 2 => (DateToken::Year2, 2),
                'M' => match run.min(4) {
                    4 => (DateToken::MonthName { long: true }, 4),
                    3 => (DateToken::MonthName { long: false }, 3),
                    n => (DateToken::Month, n),
                },
                'D' => (DateToken::Day, run.min(2)),
                'H' => (DateToken::Hour, run.min(2)),
                'm' => (DateToken::Minute, run.min(2)),
                's' => (DateToken::Second, run.min(2)),
                'X' => (DateToken::UnixSeconds, 1),
                'x' => (DateToken::UnixMillis, 1),
                '[' => {
                    if let Some(len) = chars[i + 1..].iter().position(|&x| x == ']') {
                        tokens.extend(
                            chars[i + 1..i + 1 + len]
                                .iter()
                                .map(|&x| DateToken::Literal(x)),
                        );
                        i += len + 2;
                        continue;
                    }
                    (DateToken::Literal('['), 1)
                }
                other => (DateToken::Literal(other), 1),
            };
            tokens.push(token);
            i += len;
        }
        Self { tokens }
    }

    /// Parse `text` completely; `None` when it does not match the pattern or
    /// names a date that does not exist (February 30, month 13, hour 25).
    pub(crate) fn parse(&self, text: &str) -> Option<f64> {
        let mut rest = text.trim();
        let (mut year, mut month, mut day) = (1970_i64, 1_i64, 1_i64);
        let (mut hour, mut minute, mut second) = (0_i64, 0_i64, 0_i64);
        let mut unix_ms: Option<f64> = None;
        for token in &self.tokens {
            match token {
                DateToken::Literal(c) => rest = rest.strip_prefix(*c)?,
                DateToken::Year4 => (year, rest) = take_number(rest, 4, 4)?,
                DateToken::Year2 => {
                    let (value, tail) = take_number(rest, 2, 2)?;
                    year = if value > 68 {
                        1900 + value
                    } else {
                        2000 + value
                    };
                    rest = tail;
                }
                DateToken::MonthName { long } => (month, rest) = take_month_name(rest, *long)?,
                DateToken::Month => (month, rest) = take_number(rest, 1, 2)?,
                DateToken::Day => (day, rest) = take_number(rest, 1, 2)?,
                DateToken::Hour => (hour, rest) = take_number(rest, 1, 2)?,
                DateToken::Minute => (minute, rest) = take_number(rest, 1, 2)?,
                DateToken::Second => (second, rest) = take_number(rest, 1, 2)?,
                DateToken::UnixSeconds => {
                    let (value, tail) = take_number(rest, 1, 15)?;
                    unix_ms = Some(value as f64 * 1000.0);
                    rest = tail;
                }
                DateToken::UnixMillis => {
                    let (value, tail) = take_number(rest, 1, 15)?;
                    unix_ms = Some(value as f64);
                    rest = tail;
                }
            }
        }
        if !rest.is_empty() {
            return None;
        }
        if let Some(ms) = unix_ms {
            return Some(ms / MS_PER_DAY);
        }
        if !(1..=12).contains(&month)
            || day < 1
            || day > i64::from(days_in_month(year, month as u32))
            || hour > 23
            || minute > 59
            || second > 59
        {
            return None;
        }
        let seconds = hour * 3600 + minute * 60 + second;
        Some(days_from_civil(year, month as u32, day as u32) as f64 + seconds as f64 / 86_400.0)
    }
}

/// Leading run of `min..=max` ASCII digits and the text after it.
fn take_number(text: &str, min: usize, max: usize) -> Option<(i64, &str)> {
    let digits = text
        .bytes()
        .take(max)
        .take_while(u8::is_ascii_digit)
        .count();
    if digits < min {
        return None;
    }
    let value = text[..digits].parse().ok()?;
    Some((value, &text[digits..]))
}

/// Case-insensitive month name (full or three-letter) and the text after it.
fn take_month_name(text: &str, long: bool) -> Option<(i64, &str)> {
    for (index, name) in MONTH_NAMES.iter().enumerate() {
        let full: &str = name;
        let name = if long { full } else { &full[..3] };
        if let Some(head) = text.get(..name.len())
            && head.eq_ignore_ascii_case(name)
        {
            return Some((index as i64 + 1, &text[name.len()..]));
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurationUnit {
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

/// A task length such as `30d`, `1.5w` or `15m`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Duration {
    pub(crate) amount: f64,
    pub(crate) unit: DurationUnit,
}

/// Parse a Mermaid duration: a number and one of `ms s m h d w M y`. Following
/// Mermaid, `m` is minutes and `M` is months; `s h d w y` also accept upper case.
pub(crate) fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    if !number.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    let amount: f64 = number.parse().ok()?;
    let unit = match unit.trim() {
        "ms" => DurationUnit::Millisecond,
        "s" | "S" => DurationUnit::Second,
        "m" => DurationUnit::Minute,
        "M" => DurationUnit::Month,
        "h" | "H" => DurationUnit::Hour,
        "d" | "D" => DurationUnit::Day,
        "w" | "W" => DurationUnit::Week,
        "y" | "Y" => DurationUnit::Year,
        _ => return None,
    };
    Some(Duration {
        amount: amount.min(MAX_DURATION_AMOUNT),
        unit,
    })
}

/// `start` moved forward by `duration`. Months and years are calendar months
/// (January 31 plus one month is the last day of February).
pub(crate) fn add_duration(start: f64, duration: Duration) -> f64 {
    let amount = duration.amount;
    match duration.unit {
        DurationUnit::Millisecond => start + amount / MS_PER_DAY,
        DurationUnit::Second => start + amount / 86_400.0,
        DurationUnit::Minute => start + amount / 1440.0,
        DurationUnit::Hour => start + amount / 24.0,
        DurationUnit::Day => start + amount,
        DurationUnit::Week => start + amount * 7.0,
        DurationUnit::Month => add_months(start, amount),
        DurationUnit::Year => add_months(start, amount * 12.0),
    }
}

fn add_months(start: f64, months: f64) -> f64 {
    let whole = months.trunc();
    let fraction = months - whole;
    let day_number = start.floor();
    let time_of_day = start - day_number;
    let (year, month, day) = civil_from_days(day_number as i64);
    let index = year * 12 + i64::from(month) - 1 + whole as i64;
    let (new_year, new_month) = (index.div_euclid(12), index.rem_euclid(12) as u32 + 1);
    let new_day = day.min(days_in_month(new_year, new_month));
    days_from_civil(new_year, new_month, new_day) as f64 + time_of_day + fraction * 30.0
}

/// Format `instant` with a d3 `timeFormat` pattern (Mermaid's `axisFormat`).
///
/// Supported: `%Y %y %m %d %e %b %B %a %A %H %I %M %S %L %p %j %u %w %U %W %Z %%`
/// and the `-` `_` `0` padding modifiers (`%-d`). An unknown directive prints
/// its letter, as d3 does.
pub(crate) fn format_instant(pattern: &str, instant: f64) -> String {
    let total_ms = (instant * MS_PER_DAY).round() as i64;
    let days = total_ms.div_euclid(MS_PER_DAY_I64);
    let ms_of_day = total_ms.rem_euclid(MS_PER_DAY_I64);
    let (year, month, day) = civil_from_days(days);
    let hour = ms_of_day / 3_600_000;
    let minute = ms_of_day / 60_000 % 60;
    let second = ms_of_day / 1000 % 60;
    let millis = ms_of_day % 1000;
    let weekday = (days + 4).rem_euclid(7);
    let year_day = days - days_from_civil(year, 1, 1);

    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(mut spec) = chars.next() else {
            out.push('%');
            break;
        };
        let mut pad = None;
        if matches!(spec, '-' | '_' | '0') {
            pad = Some(spec);
            let Some(next) = chars.next() else { break };
            spec = next;
        }
        match spec {
            'Y' => out.push_str(&padded(year.rem_euclid(10_000), 4, '0', pad)),
            'y' => out.push_str(&padded(year.rem_euclid(100), 2, '0', pad)),
            'm' => out.push_str(&padded(i64::from(month), 2, '0', pad)),
            'd' => out.push_str(&padded(i64::from(day), 2, '0', pad)),
            'e' => out.push_str(&padded(i64::from(day), 2, ' ', pad)),
            'H' => out.push_str(&padded(hour, 2, '0', pad)),
            'I' => out.push_str(&padded((hour + 11) % 12 + 1, 2, '0', pad)),
            'M' => out.push_str(&padded(minute, 2, '0', pad)),
            'S' => out.push_str(&padded(second, 2, '0', pad)),
            'L' => out.push_str(&padded(millis, 3, '0', pad)),
            'j' => out.push_str(&padded(year_day + 1, 3, '0', pad)),
            'u' => out.push_str(&((weekday + 6) % 7 + 1).to_string()),
            'w' => out.push_str(&weekday.to_string()),
            'U' => out.push_str(&padded((year_day + 7 - weekday) / 7, 2, '0', pad)),
            'W' => out.push_str(&padded((year_day + 7 - (weekday + 6) % 7) / 7, 2, '0', pad)),
            'a' => out.push_str(&WEEKDAY_NAMES[weekday as usize][..3]),
            'A' => out.push_str(WEEKDAY_NAMES[weekday as usize]),
            'b' => out.push_str(&MONTH_NAMES[month as usize - 1][..3]),
            'B' => out.push_str(MONTH_NAMES[month as usize - 1]),
            'p' => out.push_str(if hour < 12 { "AM" } else { "PM" }),
            'Z' => out.push_str("+0000"),
            other => out.push(other),
        }
    }
    out
}

/// `value` left-padded to `width` with the d3 padding rule: `default_fill`
/// unless a `-` (none), `_` (space) or `0` modifier was given.
fn padded(value: i64, width: usize, default_fill: char, modifier: Option<char>) -> String {
    let digits = value.to_string();
    let fill = match modifier {
        Some('-') => return digits,
        Some('_') => ' ',
        Some('0') => '0',
        _ => default_fill,
    };
    let mut out = String::with_capacity(width.max(digits.len()));
    out.extend(std::iter::repeat_n(
        fill,
        width.saturating_sub(digits.len()),
    ));
    out.push_str(&digits);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TickUnit {
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

/// A tick spacing: every `step` units, aligned to the calendar (day numbers,
/// Sundays, month and year starts) like d3's `timeDay.every(n)` and friends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TickInterval {
    pub(crate) unit: TickUnit,
    pub(crate) step: u32,
}

/// Parse a Mermaid `tickInterval` (`1day`, `2week`, `15minute`...). The
/// sub-minute units Mermaid also accepts are not drawable at chart scale and are rejected.
pub(crate) fn parse_tick_interval(text: &str) -> Option<TickInterval> {
    let text = text.trim();
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (count, unit) = text.split_at(split);
    let step: u32 = count.parse().ok().filter(|&n: &u32| n > 0)?;
    let unit = match unit {
        "minute" => TickUnit::Minute,
        "hour" => TickUnit::Hour,
        "day" => TickUnit::Day,
        "week" => TickUnit::Week,
        "month" => TickUnit::Month,
        _ => return None,
    };
    Some(TickInterval { unit, step })
}

/// Smallest multiple of `step` that is `>= value`.
fn ceil_to_multiple(value: i64, step: i64) -> i64 {
    (value + step - 1).div_euclid(step) * step
}

fn month_start(index: i64) -> f64 {
    days_from_civil(index.div_euclid(12), index.rem_euclid(12) as u32 + 1, 1) as f64
}

fn year_start(year: i64) -> f64 {
    days_from_civil(year, 1, 1) as f64
}

/// Collect `at(index)` for `index, index + step, ...` while it is `<= end`;
/// `None` when that would exceed [`MAX_TICKS`].
fn collect_ticks(mut index: i64, step: i64, end: f64, at: impl Fn(i64) -> f64) -> Option<Vec<f64>> {
    let mut out = Vec::new();
    loop {
        let tick = at(index);
        if tick > end + EPS {
            return Some(out);
        }
        if out.len() >= MAX_TICKS {
            return None;
        }
        out.push(tick);
        index += step;
    }
}

impl TickInterval {
    /// The tick instants in `start..=end`; `None` when there would be more than 200.
    pub(crate) fn ticks(self, start: f64, end: f64) -> Option<Vec<f64>> {
        let step = i64::from(self.step);
        match self.unit {
            TickUnit::Minute | TickUnit::Hour | TickUnit::Day => {
                let unit_days = match self.unit {
                    TickUnit::Minute => 1.0 / 1440.0,
                    TickUnit::Hour => 1.0 / 24.0,
                    _ => 1.0,
                };
                let len = unit_days * step as f64;
                let first = (start / len - EPS).ceil() as i64;
                collect_ticks(first, 1, end, |k| k as f64 * len)
            }
            TickUnit::Week => {
                // Weeks start on Sunday (d3's `timeWeek`); day 0 (1970-01-01) is a
                // Thursday, so Sundays are the day numbers 3 + 7k.
                let first = ceil_to_multiple(((start - 3.0 - EPS) / 7.0).ceil() as i64, step);
                collect_ticks(first, step, end, |w| (3 + 7 * w) as f64)
            }
            TickUnit::Month => {
                let (year, month, _) = civil_from_days(start.floor() as i64);
                let mut first = year * 12 + i64::from(month) - 1;
                if month_start(first) < start - EPS {
                    first += 1;
                }
                collect_ticks(ceil_to_multiple(first, step), step, end, month_start)
            }
            TickUnit::Year => {
                let (mut first, _, _) = civil_from_days(start.floor() as i64);
                if year_start(first) < start - EPS {
                    first += 1;
                }
                collect_ticks(ceil_to_multiple(first, step), step, end, year_start)
            }
        }
    }
}

const fn tick(unit: TickUnit, step: u32) -> TickInterval {
    TickInterval { unit, step }
}

/// Candidate spacings from fine to coarse for [`auto_ticks`].
const AUTO_INTERVALS: [TickInterval; 25] = [
    tick(TickUnit::Minute, 1),
    tick(TickUnit::Minute, 5),
    tick(TickUnit::Minute, 15),
    tick(TickUnit::Minute, 30),
    tick(TickUnit::Hour, 1),
    tick(TickUnit::Hour, 3),
    tick(TickUnit::Hour, 6),
    tick(TickUnit::Hour, 12),
    tick(TickUnit::Day, 1),
    tick(TickUnit::Day, 2),
    tick(TickUnit::Week, 1),
    tick(TickUnit::Week, 2),
    tick(TickUnit::Month, 1),
    tick(TickUnit::Month, 3),
    tick(TickUnit::Month, 6),
    tick(TickUnit::Year, 1),
    tick(TickUnit::Year, 2),
    tick(TickUnit::Year, 5),
    tick(TickUnit::Year, 10),
    tick(TickUnit::Year, 20),
    tick(TickUnit::Year, 50),
    tick(TickUnit::Year, 100),
    tick(TickUnit::Year, 200),
    tick(TickUnit::Year, 500),
    tick(TickUnit::Year, 1000),
];

/// The finest calendar spacing whose ticks in `start..=end` number at least two
/// and satisfy `accept` (the caller checks that the labels fit). Falls back to
/// the two end points when no spacing qualifies.
pub(crate) fn auto_ticks(start: f64, end: f64, accept: impl Fn(&[f64]) -> bool) -> Vec<f64> {
    for interval in AUTO_INTERVALS {
        if let Some(ticks) = interval.ticks(start, end)
            && ticks.len() >= 2
            && accept(&ticks)
        {
            return ticks;
        }
    }
    vec![start, end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ymd(text: &str) -> f64 {
        DateFormat::default().parse(text).unwrap()
    }

    fn labels(pattern: &str, ticks: &[f64]) -> Vec<String> {
        ticks.iter().map(|&t| format_instant(pattern, t)).collect()
    }

    #[test]
    fn civil_dates_round_trip_through_day_numbers() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2014, 1, 1), 16071);
        assert_eq!(civil_from_days(16071), (2014, 1, 1));
        assert_eq!(
            days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 28),
            2
        );
        assert_eq!(
            days_from_civil(1900, 3, 1) - days_from_civil(1900, 2, 28),
            1
        );
        for days in (-800_000..800_000).step_by(37) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "day {days} -> {y}-{m}-{d}");
        }
    }

    #[test]
    fn default_date_format_is_iso_and_rejects_non_dates() {
        assert_eq!(ymd("2014-01-31"), 16101.0);
        assert_eq!(ymd(" 2014-1-5 "), 16075.0);
        let iso = DateFormat::default();
        for bad in [
            "2014-02-30",
            "2014-13-01",
            "2014-00-10",
            "task-1",
            "2014-01-01x",
            "2014",
            "30d",
            "",
        ] {
            assert_eq!(iso.parse(bad), None, "{bad:?}");
        }
        assert_eq!(
            iso.parse("2012-02-29"),
            Some(days_from_civil(2012, 2, 29) as f64)
        );
        assert_eq!(iso.parse("2013-02-29"), None);
    }

    #[test]
    fn custom_date_formats_parse_the_same_day() {
        let jan31 = 16101.0;
        for (pattern, text) in [
            ("DD-MM-YYYY", "31-01-2014"),
            ("MM/DD/YY", "01/31/14"),
            ("DD MMM YYYY", "31 jan 2014"),
            ("MMMM D, YYYY", "January 31, 2014"),
            ("YYYYMMDD", "20140131"),
            ("YYYY.MM.DD", "2014.01.31"),
            ("X", "1391126400"),
            ("x", "1391126400000"),
        ] {
            assert_eq!(
                DateFormat::new(pattern).parse(text),
                Some(jan31),
                "{pattern} / {text}"
            );
        }
        assert_eq!(DateFormat::new("YY-MM-DD").parse("69-01-01"), Some(-365.0));
        assert_eq!(DateFormat::new("[Q]YYYY").parse("Q2014"), Some(16071.0));
        assert_eq!(DateFormat::new("[Q]YYYY").parse("2014"), None);
        let with_time = DateFormat::new("YYYY-MM-DD HH:mm");
        assert_eq!(
            with_time.parse("2014-01-01 06:30"),
            Some(16071.0 + 6.5 / 24.0)
        );
        assert_eq!(with_time.parse("2014-01-01 24:00"), None);
        assert_eq!(with_time.parse("2014-01-01 10:60"), None);
        assert_eq!(DateFormat::new("HH:mm").parse("12:00"), Some(0.5));
    }

    #[test]
    fn durations_follow_mermaid_units() {
        let parsed = |text| parse_duration(text).map(|d| (d.amount, d.unit));
        assert_eq!(parsed("30d"), Some((30.0, DurationUnit::Day)));
        assert_eq!(parsed("1.5w"), Some((1.5, DurationUnit::Week)));
        assert_eq!(parsed("2M"), Some((2.0, DurationUnit::Month)));
        assert_eq!(parsed("15m"), Some((15.0, DurationUnit::Minute)));
        assert_eq!(parsed("500ms"), Some((500.0, DurationUnit::Millisecond)));
        assert_eq!(parsed("3 y"), Some((3.0, DurationUnit::Year)));
        assert_eq!(
            parsed("99999999999d").map(|d| d.0),
            Some(1.0e6),
            "amount is capped"
        );
        for bad in ["d1", "a1", "m1", "5", "", "1.2.3d", "1e5d", "30days", "."] {
            assert_eq!(parsed(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn adding_durations_uses_the_calendar_for_months_and_years() {
        let dur = |text| parse_duration(text).unwrap();
        assert_eq!(
            add_duration(ymd("2014-01-01"), dur("30d")),
            ymd("2014-01-31")
        );
        assert_eq!(
            add_duration(ymd("2014-01-01"), dur("2w")),
            ymd("2014-01-15")
        );
        assert_eq!(
            add_duration(ymd("2014-01-31"), dur("1M")),
            ymd("2014-02-28")
        );
        assert_eq!(
            add_duration(ymd("2014-11-30"), dur("3M")),
            ymd("2015-02-28")
        );
        assert_eq!(
            add_duration(ymd("2012-02-29"), dur("1y")),
            ymd("2013-02-28")
        );
        assert_eq!(add_duration(ymd("2014-01-01"), dur("12h")), 16071.5);
        assert_eq!(add_duration(ymd("2014-01-01"), dur("90m")), 16071.0625);
        assert_eq!(add_duration(ymd("2014-01-01"), dur("0d")), 16071.0);
    }

    #[test]
    fn axis_format_directives_print_the_requested_fields() {
        let stamp = DateFormat::new("YYYY-MM-DD HH:mm:ss")
            .parse("2014-01-31 15:05:09")
            .unwrap();
        for (pattern, expected) in [
            ("%Y-%m-%d", "2014-01-31"),
            ("%m/%d", "01/31"),
            ("%b %d", "Jan 31"),
            ("%B %e, %Y", "January 31, 2014"),
            ("%a %A", "Fri Friday"),
            ("%H:%M:%S", "15:05:09"),
            ("%I:%M %p", "03:05 PM"),
            ("%j", "031"),
            ("%-d/%-m/%y", "31/1/14"),
            ("%_m|%0e", " 1|31"),
            ("%u %w %L %Z", "5 5 000 +0000"),
            ("100%%", "100%"),
            ("%Q", "Q"),
            ("end%", "end%"),
        ] {
            assert_eq!(format_instant(pattern, stamp), expected, "{pattern}");
        }
        let sunday = ymd("2014-01-05");
        assert_eq!(format_instant("%e|%u %w|%U %W", sunday), " 5|7 0|01 00");
        assert_eq!(format_instant("%I %p", sunday), "12 AM");
        assert_eq!(
            format_instant("%Y-%m-%d", 16_071.999_999_999_9),
            "2014-01-02",
            "rounds to the millisecond"
        );
    }

    #[test]
    fn weekly_ticks_fall_on_sundays() {
        let ticks = parse_tick_interval("1week")
            .unwrap()
            .ticks(ymd("2014-01-01"), ymd("2014-02-20"))
            .unwrap();
        assert_eq!(
            labels("%Y-%m-%d", &ticks),
            [
                "2014-01-05",
                "2014-01-12",
                "2014-01-19",
                "2014-01-26",
                "2014-02-02",
                "2014-02-09",
                "2014-02-16"
            ]
        );
        assert!(labels("%a", &ticks).iter().all(|d| d == "Sun"));
    }

    #[test]
    fn monthly_and_yearly_ticks_are_calendar_aligned() {
        let months = TickInterval {
            unit: TickUnit::Month,
            step: 1,
        };
        let ticks = months.ticks(ymd("2014-01-15"), ymd("2014-06-01")).unwrap();
        assert_eq!(
            labels("%Y-%m-%d", &ticks),
            [
                "2014-02-01",
                "2014-03-01",
                "2014-04-01",
                "2014-05-01",
                "2014-06-01"
            ]
        );
        let every_other = TickInterval {
            unit: TickUnit::Month,
            step: 2,
        };
        let ticks = every_other
            .ticks(ymd("2014-01-15"), ymd("2014-06-01"))
            .unwrap();
        assert_eq!(labels("%Y-%m-%d", &ticks), ["2014-03-01", "2014-05-01"]);
        let years = TickInterval {
            unit: TickUnit::Year,
            step: 1,
        };
        let ticks = years.ticks(ymd("2013-06-01"), ymd("2016-06-01")).unwrap();
        assert_eq!(labels("%Y", &ticks), ["2014", "2015", "2016"]);
        let ticks = TickInterval {
            unit: TickUnit::Year,
            step: 2,
        }
        .ticks(ymd("2013-06-01"), ymd("2016-06-01"))
        .unwrap();
        assert_eq!(labels("%Y", &ticks), ["2014", "2016"]);
    }

    #[test]
    fn day_hour_and_minute_ticks_include_boundaries() {
        let days = TickInterval {
            unit: TickUnit::Day,
            step: 1,
        };
        let ticks = days.ticks(ymd("2014-01-01"), ymd("2014-01-04")).unwrap();
        assert_eq!(labels("%d", &ticks), ["01", "02", "03", "04"]);
        let clock = DateFormat::new("YYYY-MM-DD HH:mm");
        let hours = TickInterval {
            unit: TickUnit::Hour,
            step: 1,
        };
        let ticks = hours
            .ticks(
                clock.parse("2014-01-01 00:30").unwrap(),
                clock.parse("2014-01-01 03:30").unwrap(),
            )
            .unwrap();
        assert_eq!(labels("%H:%M", &ticks), ["01:00", "02:00", "03:00"]);
        let quarters = TickInterval {
            unit: TickUnit::Minute,
            step: 15,
        };
        let ticks = quarters
            .ticks(
                clock.parse("2014-01-01 00:10").unwrap(),
                clock.parse("2014-01-01 00:50").unwrap(),
            )
            .unwrap();
        assert_eq!(labels("%H:%M", &ticks), ["00:15", "00:30", "00:45"]);
    }

    #[test]
    fn tick_candidates_with_too_many_ticks_are_rejected() {
        let minutes = TickInterval {
            unit: TickUnit::Minute,
            step: 1,
        };
        assert_eq!(minutes.ticks(ymd("2014-01-01"), ymd("2014-01-11")), None);
        let years = TickInterval {
            unit: TickUnit::Year,
            step: 1,
        };
        assert_eq!(
            years.ticks(0.0, f64::INFINITY),
            None,
            "an unbounded range still terminates"
        );
    }

    #[test]
    fn tick_intervals_parse_like_mermaid() {
        assert_eq!(
            parse_tick_interval("1week"),
            Some(TickInterval {
                unit: TickUnit::Week,
                step: 1
            })
        );
        assert_eq!(
            parse_tick_interval(" 15minute "),
            Some(TickInterval {
                unit: TickUnit::Minute,
                step: 15
            })
        );
        assert_eq!(
            parse_tick_interval("2month"),
            Some(TickInterval {
                unit: TickUnit::Month,
                step: 2
            })
        );
        for bad in [
            "0day",
            "week",
            "3fortnight",
            "1second",
            "1millisecond",
            "",
            "-1day",
        ] {
            assert_eq!(parse_tick_interval(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn auto_ticks_pick_the_finest_spacing_that_is_accepted() {
        let (start, end) = (ymd("2014-01-01"), ymd("2014-02-20"));
        let weekly = auto_ticks(start, end, |t| t.len() <= 8);
        assert_eq!(weekly.len(), 7);
        assert_eq!(labels("%Y-%m-%d %a", &weekly)[0], "2014-01-05 Sun");
        let fortnightly = auto_ticks(start, end, |t| t.len() <= 4);
        assert_eq!(
            labels("%m/%d", &fortnightly),
            ["01/05", "01/19", "02/02", "02/16"]
        );
        assert_eq!(auto_ticks(start, end, |_| false), vec![start, end]);
    }
}
