//! UI language and locale-aware formatting of numbers, dates and file sizes.

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::Globalization::{
    DATE_LONGDATE, DATE_SHORTDATE, ENUM_DATE_FORMATS_FLAGS, GetDateFormatEx, GetLocaleInfoEx,
    GetNumberFormatEx, GetUserDefaultLocaleName, GetUserDefaultUILanguage, LCIDToLocaleName,
    LOCALE_ILZERO, LOCALE_INEGNUMBER, LOCALE_SDECIMAL, LOCALE_SGROUPING, LOCALE_STHOUSAND,
    NUMBERFMTW,
};
use windows::Win32::System::SystemServices::LOCALE_NAME_MAX_LENGTH;
use windows::core::{PCWSTR, PWSTR};

use crate::com::{from_wide, to_wide};

/// `LCIDToLocaleName` flag: do not return alternate sort names.
const LOCALE_ALLOW_NEUTRAL_NAMES: u32 = 0x0800_0000;
const SORT_DEFAULT: u32 = 0;

/// The locale used when Windows names none.
const FALLBACK_TAG: &str = "en-US";

/// Binary prefixes used for file sizes.
const SIZE_UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

/// The BCP-47 tag for a Windows language id (for example `0x041F` -> `tr-TR`), or `None` when
/// the id has no locale name.
#[must_use]
pub fn tag_from_langid(langid: u16) -> Option<String> {
    let lcid = (SORT_DEFAULT << 16) | u32::from(langid);
    let mut buffer = [0u16; LOCALE_NAME_MAX_LENGTH as usize];
    // SAFETY: the buffer is writable for its full length.
    let written = unsafe { LCIDToLocaleName(lcid, Some(&mut buffer), LOCALE_ALLOW_NEUTRAL_NAMES) };
    (written > 0).then(|| from_wide(&buffer))
}

/// The user's UI language as a BCP-47 tag (`en-US` when Windows reports none).
#[must_use]
pub fn ui_language() -> String {
    // SAFETY: pure query.
    let langid = unsafe { GetUserDefaultUILanguage() };
    tag_or_fallback(langid)
}

/// [`tag_from_langid`], or [`FALLBACK_TAG`] for an id without a locale name.
fn tag_or_fallback(langid: u16) -> String {
    tag_from_langid(langid).unwrap_or_else(|| FALLBACK_TAG.to_owned())
}

/// The NLS `Grouping` value for an `SGROUPING` string: `"3;0"` -> 3, `"3;2;0"` -> 32,
/// `"3;2"` -> 320.
#[must_use]
pub fn grouping_value(sgrouping: &str) -> u32 {
    let mut parts: Vec<&str> = sgrouping
        .split(';')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.last() == Some(&"0") {
        parts.pop();
    } else {
        parts.push("0");
    }
    parts.concat().parse().unwrap_or(0)
}

/// A locale for formatting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locale {
    tag: String,
    name: Vec<u16>,
}

impl Locale {
    /// The locale with BCP-47 `tag` (for example `tr-TR`).
    #[must_use]
    pub fn new(tag: &str) -> Self {
        Self {
            tag: tag.to_owned(),
            name: to_wide(tag),
        }
    }

    /// The user's default formatting locale.
    #[must_use]
    pub fn user_default() -> Self {
        let mut buffer = [0u16; LOCALE_NAME_MAX_LENGTH as usize];
        // SAFETY: the buffer is writable for its full length.
        let written = unsafe { GetUserDefaultLocaleName(&mut buffer) };
        Self::named_by_system(written, &buffer)
    }

    /// The locale `GetUserDefaultLocaleName` wrote into `buffer`; it returns 0 characters when
    /// the user has none, which is [`FALLBACK_TAG`].
    fn named_by_system(written: i32, buffer: &[u16]) -> Self {
        if written > 0 {
            Self::new(&from_wide(buffer))
        } else {
            Self::new(FALLBACK_TAG)
        }
    }

    #[must_use]
    pub fn tag(&self) -> &str {
        &self.tag
    }

    fn name(&self) -> PCWSTR {
        PCWSTR(self.name.as_ptr())
    }

    fn info(&self, kind: u32) -> Option<String> {
        let mut buffer = [0u16; 80];
        // SAFETY: valid locale name and writable buffer.
        let written = unsafe { GetLocaleInfoEx(self.name(), kind, Some(&mut buffer)) };
        (written > 0).then(|| from_wide(&buffer))
    }

    /// Formats a decimal string (`-?digits[.digits]`) with the locale's separators and
    /// `digits` fraction digits.
    fn format_numeric(&self, value: &str, digits: u32) -> Option<String> {
        // The five settings or none: a locale the system knows has them all.
        let ((((decimal, thousand), grouping), leading_zero), negative_order) = self
            .info(LOCALE_SDECIMAL)
            .zip(self.info(LOCALE_STHOUSAND))
            .zip(self.info(LOCALE_SGROUPING))
            .zip(self.info(LOCALE_ILZERO))
            .zip(self.info(LOCALE_INEGNUMBER))?;
        let mut decimal = to_wide(&decimal);
        let mut thousand = to_wide(&thousand);
        let grouping = grouping_value(&grouping);
        let leading_zero = leading_zero.parse().unwrap_or(1);
        let negative_order = negative_order.parse().unwrap_or(1);
        let format = NUMBERFMTW {
            NumDigits: digits,
            LeadingZero: leading_zero,
            Grouping: grouping,
            lpDecimalSep: PWSTR(decimal.as_mut_ptr()),
            lpThousandSep: PWSTR(thousand.as_mut_ptr()),
            NegativeOrder: negative_order,
        };
        let value = to_wide(value);
        let mut out = [0u16; 128];
        // SAFETY: all pointers reference live NUL-terminated buffers for the call duration.
        let written = unsafe {
            GetNumberFormatEx(
                self.name(),
                0,
                PCWSTR(value.as_ptr()),
                Some(&format),
                Some(&mut out),
            )
        };
        (written > 0).then(|| from_wide(&out))
    }

    /// An integer with digit grouping (`1,234,567` / `1.234.567`).
    #[must_use]
    pub fn format_integer(&self, value: i64) -> String {
        self.format_numeric(&value.to_string(), 0)
            .unwrap_or_else(|| value.to_string())
    }

    /// A number with `digits` fraction digits and the locale's separators.
    #[must_use]
    pub fn format_decimal(&self, value: f64, digits: u32) -> String {
        let plain = format!("{value:.digits$}", digits = digits as usize);
        self.format_numeric(&plain, digits).unwrap_or(plain)
    }

    fn format_date(
        &self,
        flags: ENUM_DATE_FORMATS_FLAGS,
        year: u16,
        month: u16,
        day: u16,
    ) -> Option<String> {
        let date = SYSTEMTIME {
            wYear: year,
            wMonth: month,
            wDayOfWeek: 0,
            wDay: day,
            wHour: 0,
            wMinute: 0,
            wSecond: 0,
            wMilliseconds: 0,
        };
        let mut out = [0u16; 128];
        // SAFETY: valid locale name, date and writable buffer; no format string or calendar.
        let written = unsafe {
            GetDateFormatEx(
                self.name(),
                flags,
                Some(&date),
                PCWSTR::null(),
                Some(&mut out),
                PCWSTR::null(),
            )
        };
        (written > 0).then(|| from_wide(&out))
    }

    /// The long date (`Saturday, October 10, 2026`), or `None` for an invalid date.
    #[must_use]
    pub fn format_date_long(&self, year: u16, month: u16, day: u16) -> Option<String> {
        self.format_date(DATE_LONGDATE, year, month, day)
    }

    /// The short date (`10/10/2026`), or `None` for an invalid date.
    #[must_use]
    pub fn format_date_short(&self, year: u16, month: u16, day: u16) -> Option<String> {
        self.format_date(DATE_SHORTDATE, year, month, day)
    }

    /// A file size with binary units: `512 B`, `1.5 KB`, `3.0 MB`.
    #[must_use]
    pub fn format_file_size(&self, bytes: u64) -> String {
        if bytes < 1024 {
            return format!("{} B", self.format_integer(bytes as i64));
        }
        let mut value = bytes as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit + 1 < SIZE_UNITS.len() {
            value /= 1024.0;
            unit += 1;
        }
        format!("{} {}", self.format_decimal(value, 1), SIZE_UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_ids_map_to_bcp47_tags() {
        assert_eq!(tag_from_langid(0x041F).as_deref(), Some("tr-TR"));
        assert_eq!(tag_from_langid(0x0409).as_deref(), Some("en-US"));
        assert_eq!(tag_from_langid(0x0407).as_deref(), Some("de-DE"));
        assert_eq!(tag_from_langid(0xFFFF), None);
        let ui = ui_language();
        assert!(
            ui.len() >= 2 && ui.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{ui}"
        );
    }

    #[test]
    fn a_language_without_a_locale_name_falls_back_to_american_english() {
        assert_eq!(tag_or_fallback(0x041F), "tr-TR");
        assert_eq!(tag_or_fallback(0xFFFF), "en-US");
    }

    #[test]
    fn the_user_locale_comes_from_what_the_system_wrote() {
        let written = to_wide("tr-TR");
        // The count includes the terminating NUL, as the system's does.
        assert_eq!(Locale::named_by_system(6, &written).tag(), "tr-TR");
        // Nothing written: the buffer's content is not a locale.
        assert_eq!(Locale::named_by_system(0, &written).tag(), "en-US");
        assert!(!Locale::user_default().tag().is_empty());
    }

    #[test]
    fn grouping_strings_convert_to_nls_values() {
        for (input, expected) in [("3;0", 3), ("3;2;0", 32), ("3;2", 320), ("0", 0), ("", 0)] {
            assert_eq!(grouping_value(input), expected, "{input:?}");
        }
    }

    #[test]
    fn integers_use_the_locale_separators() {
        let en = Locale::new("en-US");
        let tr = Locale::new("tr-TR");
        assert_eq!(en.format_integer(1_234_567), "1,234,567");
        assert_eq!(tr.format_integer(1_234_567), "1.234.567");
        assert_eq!(en.format_integer(-1234), "-1,234");
        assert_eq!(en.format_integer(12), "12");
        assert_eq!(en.tag(), "en-US");
    }

    #[test]
    fn decimals_use_the_locale_separators() {
        let en = Locale::new("en-US");
        let tr = Locale::new("tr-TR");
        assert_eq!(en.format_decimal(1234.5, 1), "1,234.5");
        assert_eq!(tr.format_decimal(1234.5, 1), "1.234,5");
        assert_eq!(en.format_decimal(0.25, 2), "0.25");
        assert_eq!(tr.format_decimal(2.0, 0), "2");
    }

    #[test]
    fn dates_follow_the_locale() {
        let en = Locale::new("en-US");
        let tr = Locale::new("tr-TR");
        assert_eq!(
            en.format_date_long(2026, 10, 10).as_deref(),
            Some("Saturday, October 10, 2026")
        );
        assert_eq!(
            en.format_date_short(2026, 10, 10).as_deref(),
            Some("10/10/2026")
        );
        let long = tr.format_date_long(2026, 10, 10).unwrap();
        assert!(
            long.contains("Ekim") && long.contains("Cumartesi"),
            "{long}"
        );
        assert_eq!(
            tr.format_date_short(2026, 10, 10).as_deref(),
            Some("10.10.2026")
        );
        assert_eq!(en.format_date_long(2026, 13, 40), None);
        assert_eq!(en.format_date_short(2026, 2, 31), None);
    }

    #[test]
    fn file_sizes_pick_binary_units() {
        let en = Locale::new("en-US");
        let tr = Locale::new("tr-TR");
        assert_eq!(en.format_file_size(0), "0 B");
        assert_eq!(en.format_file_size(1023), "1,023 B");
        assert_eq!(en.format_file_size(1024), "1.0 KB");
        assert_eq!(en.format_file_size(1536), "1.5 KB");
        assert_eq!(tr.format_file_size(1536), "1,5 KB");
        assert_eq!(en.format_file_size(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(en.format_file_size(5 * 1024 * 1024 * 1024), "5.0 GB");
        assert_eq!(en.format_file_size(u64::MAX), "16,777,216.0 TB");
    }

    #[test]
    fn the_user_default_locale_is_usable() {
        let locale = Locale::user_default();
        assert!(!locale.tag().is_empty());
        assert!(!locale.format_integer(1000).is_empty());
    }

    #[test]
    fn unknown_locales_fall_back_to_plain_digits() {
        let bogus = Locale::new("zz-not-a-locale-at-all-123456");
        assert_eq!(bogus.format_integer(1234), "1234");
        assert_eq!(bogus.format_decimal(1.5, 1), "1.5");
        assert_eq!(bogus.format_date_long(2026, 1, 1), None);
    }
}
