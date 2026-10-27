use time::{OffsetDateTime, UtcOffset};

/// The formatting style for a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampFormat {
    /// Formats the timestamp as an absolute time, e.g. "2021-12-31 3:00AM".
    Absolute,
    /// Formats the timestamp as an absolute time.
    /// If the message is from today or yesterday the date will be replaced with "Today at x" or "Yesterday at x" respectively.
    /// E.g. "Today at 12:00 PM", "Yesterday at 11:00 AM", "2021-12-31 3:00AM".
    EnhancedAbsolute,
    /// Formats the timestamp as an absolute time, using month name, day of month, year. e.g. "Feb. 24, 2024".
    MediumAbsolute,
    /// Formats the timestamp as a relative time, e.g. "just now", "1 minute ago", "2 hours ago", "2 months ago".
    Relative,
}

/// Formats a timestamp, which respects the user's date and time preferences/custom format.
pub fn format_localized_timestamp(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    timezone: UtcOffset,
    format: TimestampFormat,
) -> String {
    let timestamp_local = timestamp.to_offset(timezone);
    let reference_local = reference.to_offset(timezone);
    format_local_timestamp(timestamp_local, reference_local, format)
}

/// Formats a timestamp, which respects the user's date and time preferences/custom format.
pub fn format_local_timestamp(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    format: TimestampFormat,
) -> String {
    match format {
        TimestampFormat::Absolute => format_absolute_timestamp(timestamp, reference, false),
        TimestampFormat::EnhancedAbsolute => format_absolute_timestamp(timestamp, reference, true),
        TimestampFormat::MediumAbsolute => format_absolute_timestamp_medium(timestamp, reference),
        TimestampFormat::Relative => format_relative_time(timestamp, reference)
            .unwrap_or_else(|| format_relative_date(timestamp, reference)),
    }
}

/// Formats the date component of a timestamp
pub fn format_date(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    enhanced_formatting: bool,
) -> String {
    format_absolute_date(timestamp, reference, enhanced_formatting)
}

/// Formats the time component of a timestamp
pub fn format_time(timestamp: OffsetDateTime) -> String {
    format_absolute_time(timestamp)
}

/// Formats the date component of a timestamp in medium style
pub fn format_date_medium(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    enhanced_formatting: bool,
) -> String {
    format_absolute_date_medium(timestamp, reference, enhanced_formatting)
}

fn format_absolute_date(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    #[allow(unused_variables)] enhanced_date_formatting: bool,
) -> String {
    #[cfg(target_os = "macos")]
    {
        if !enhanced_date_formatting {
            return macos::format_date(&timestamp);
        }

        let timestamp_date = timestamp.date();
        let reference_date = reference.date();
        if timestamp_date == reference_date {
            "Today".to_string()
        } else if reference_date.previous_day() == Some(timestamp_date) {
            "Yesterday".to_string()
        } else {
            macos::format_date(&timestamp)
        }
    }
    #[cfg(target_os = "windows")]
    {
        if !enhanced_date_formatting {
            return windows::format_date(&timestamp);
        }

        let timestamp_date = timestamp.date();
        let reference_date = reference.date();
        if timestamp_date == reference_date {
            "Today".to_string()
        } else if reference_date.previous_day() == Some(timestamp_date) {
            "Yesterday".to_string()
        } else {
            windows::format_date(&timestamp)
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // todo(linux) respect user's date/time preferences
        let current_locale = CURRENT_LOCALE
            .get_or_init(|| sys_locale::get_locale().unwrap_or_else(|| String::from("en-US")));
        format_timestamp_naive_date(
            timestamp,
            reference,
            is_12_hour_time_by_locale(current_locale.as_str()),
        )
    }
}

fn format_absolute_time(timestamp: OffsetDateTime) -> String {
    #[cfg(target_os = "macos")]
    {
        macos::format_time(&timestamp)
    }
    #[cfg(target_os = "windows")]
    {
        windows::format_time(&timestamp)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // todo(linux) respect user's date/time preferences
        let current_locale = CURRENT_LOCALE
            .get_or_init(|| sys_locale::get_locale().unwrap_or_else(|| String::from("en-US")));
        format_timestamp_naive_time(
            timestamp,
            is_12_hour_time_by_locale(current_locale.as_str()),
        )
    }
}

fn format_absolute_timestamp(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    #[allow(unused_variables)] enhanced_date_formatting: bool,
) -> String {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        if !enhanced_date_formatting {
            return format!(
                "{} {}",
                format_absolute_date(timestamp, reference, enhanced_date_formatting),
                format_absolute_time(timestamp)
            );
        }

        let timestamp_date = timestamp.date();
        let reference_date = reference.date();
        if timestamp_date == reference_date {
            format!("Today at {}", format_absolute_time(timestamp))
        } else if reference_date.previous_day() == Some(timestamp_date) {
            format!("Yesterday at {}", format_absolute_time(timestamp))
        } else {
            format!(
                "{} {}",
                format_absolute_date(timestamp, reference, enhanced_date_formatting),
                format_absolute_time(timestamp)
            )
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // todo(linux) respect user's date/time preferences
        format_timestamp_fallback(timestamp, reference)
    }
}

fn format_absolute_date_medium(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
    enhanced_formatting: bool,
) -> String {
    #[cfg(target_os = "macos")]
    {
        if !enhanced_formatting {
            return macos::format_date_medium(&timestamp);
        }

        let timestamp_date = timestamp.date();
        let reference_date = reference.date();
        if timestamp_date == reference_date {
            "Today".to_string()
        } else if reference_date.previous_day() == Some(timestamp_date) {
            "Yesterday".to_string()
        } else {
            macos::format_date_medium(&timestamp)
        }
    }
    #[cfg(target_os = "windows")]
    {
        if !enhanced_formatting {
            return windows::format_date_medium(&timestamp);
        }

        let timestamp_date = timestamp.date();
        let reference_date = reference.date();
        if timestamp_date == reference_date {
            "Today".to_string()
        } else if reference_date.previous_day() == Some(timestamp_date) {
            "Yesterday".to_string()
        } else {
            windows::format_date_medium(&timestamp)
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // todo(linux) respect user's date/time preferences
        let current_locale = CURRENT_LOCALE
            .get_or_init(|| sys_locale::get_locale().unwrap_or_else(|| String::from("en-US")));
        if !enhanced_formatting {
            return format_timestamp_naive_date_medium(
                timestamp,
                is_12_hour_time_by_locale(current_locale.as_str()),
            );
        }

        let timestamp_date = timestamp.date();
        let reference_date = reference.date();
        if timestamp_date == reference_date {
            "Today".to_string()
        } else if reference_date.previous_day() == Some(timestamp_date) {
            "Yesterday".to_string()
        } else {
            format_timestamp_naive_date_medium(
                timestamp,
                is_12_hour_time_by_locale(current_locale.as_str()),
            )
        }
    }
}

fn format_absolute_timestamp_medium(
    timestamp: OffsetDateTime,
    reference: OffsetDateTime,
) -> String {
    #[cfg(target_os = "macos")]
    {
        format_absolute_date_medium(timestamp, reference, false)
    }
    #[cfg(target_os = "windows")]
    {
        format_absolute_date_medium(timestamp, reference, false)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // todo(linux) respect user's date/time preferences
        // todo(windows) respect user's date/time preferences
        format_timestamp_fallback(timestamp, reference)
    }
}

fn format_relative_time(timestamp: OffsetDateTime, reference: OffsetDateTime) -> Option<String> {
    let difference = reference - timestamp;
    let minutes = difference.whole_minutes();
    match minutes {
        0 => Some("Just now".to_string()),
        1 => Some("1 minute ago".to_string()),
        2..=59 => Some(format!("{} minutes ago", minutes)),
        _ => {
            let hours = difference.whole_hours();
            match hours {
                1 => Some("1 hour ago".to_string()),
                2..=23 => Some(format!("{} hours ago", hours)),
                _ => None,
            }
        }
    }
}

fn format_relative_date(timestamp: OffsetDateTime, reference: OffsetDateTime) -> String {
    let timestamp_date = timestamp.date();
    let reference_date = reference.date();
    let difference = reference_date - timestamp_date;
    let days = difference.whole_days();
    match days {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        2..=6 => format!("{} days ago", days),
        _ => {
            let weeks = difference.whole_weeks();
            match weeks {
                1 => "1 week ago".to_string(),
                2..=4 => format!("{} weeks ago", weeks),
                _ => {
                    let month_diff = calculate_month_difference(timestamp, reference);
                    match month_diff {
                        0..=1 => "1 month ago".to_string(),
                        2..=11 => format!("{} months ago", month_diff),
                        // Match git's `show_date_relative` behavior: for dates under 5 years old,
                        // include both years and months so, for example, 22 months is shown as
                        // "1 year, 10 months ago" instead of being collapsed to "1 year ago".
                        12..60 => format_compound_year_month(month_diff),
                        // Beyond 5 years, round to the nearest year.
                        months => {
                            let years = (months + 6) / 12;
                            format!("{years} years ago")
                        }
                    }
                }
            }
        }
    }
}

fn format_compound_year_month(month_diff: usize) -> String {
    let years = month_diff / 12;
    let months = month_diff % 12;
    let year_unit = if years == 1 { "year" } else { "years" };
    if months == 0 {
        format!("{years} {year_unit} ago")
    } else {
        let month_unit = if months == 1 { "month" } else { "months" };
        format!("{years} {year_unit}, {months} {month_unit} ago")
    }
}

/// Calculates the difference in months between two timestamps.
/// The reference timestamp should always be greater than the timestamp.
fn calculate_month_difference(timestamp: OffsetDateTime, reference: OffsetDateTime) -> usize {
    let timestamp_year = timestamp.year();
    let reference_year = reference.year();
    let timestamp_month: u8 = timestamp.month().into();
    let reference_month: u8 = reference.month().into();

    let month_diff = if reference_month >= timestamp_month {
        reference_month as usize - timestamp_month as usize
    } else {
        12 - timestamp_month as usize + reference_month as usize
    };

    let year_diff = (reference_year - timestamp_year) as usize;
    if year_diff == 0 {
        reference_month as usize - timestamp_month as usize
    } else if month_diff == 0 {
        year_diff * 12
    } else if timestamp_month > reference_month {
        (year_diff - 1) * 12 + month_diff
    } else {
        year_diff * 12 + month_diff
    }
}

/// Formats a timestamp, which is either in 12-hour or 24-hour time format.
/// Note:
/// This function does not respect the user's date and time preferences.
/// This should only be used as a fallback mechanism when the OS time formatting fails.
fn format_timestamp_naive_time(timestamp_local: OffsetDateTime, is_12_hour_time: bool) -> String {
    let timestamp_local_hour = timestamp_local.hour();
    let timestamp_local_minute = timestamp_local.minute();

    let (hour, meridiem) = if is_12_hour_time {
        let meridiem = if timestamp_local_hour >= 12 {
            "PM"
        } else {
            "AM"
        };

        let hour_12 = match timestamp_local_hour {
            0 => 12,                              // Midnight
            13..=23 => timestamp_local_hour - 12, // PM hours
            _ => timestamp_local_hour,            // AM hours
        };

        (hour_12, Some(meridiem))
    } else {
        (timestamp_local_hour, None)
    };

    match meridiem {
        Some(meridiem) => format!("{}:{:02} {}", hour, timestamp_local_minute, meridiem),
        None => format!("{:02}:{:02}", hour, timestamp_local_minute),
    }
}

#[cfg(not(target_os = "macos"))]
fn format_timestamp_naive_date(
    timestamp_local: OffsetDateTime,
    reference_local: OffsetDateTime,
    is_12_hour_time: bool,
) -> String {
    let reference_local_date = reference_local.date();
    let timestamp_local_date = timestamp_local.date();

    if timestamp_local_date == reference_local_date {
        "Today".to_string()
    } else if reference_local_date.previous_day() == Some(timestamp_local_date) {
        "Yesterday".to_string()
    } else {
        match is_12_hour_time {
            true => format!(
                "{:02}/{:02}/{}",
                timestamp_local_date.month() as u32,
                timestamp_local_date.day(),
                timestamp_local_date.year()
            ),
            false => format!(
                "{:02}/{:02}/{}",
                timestamp_local_date.day(),
                timestamp_local_date.month() as u32,
                timestamp_local_date.year()
            ),
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn format_timestamp_naive_date_medium(
    timestamp_local: OffsetDateTime,
    is_12_hour_time: bool,
) -> String {
    let timestamp_local_date = timestamp_local.date();

    match is_12_hour_time {
        true => format!(
            "{:02}/{:02}/{}",
            timestamp_local_date.month() as u32,
            timestamp_local_date.day(),
            timestamp_local_date.year()
        ),
        false => format!(
            "{:02}/{:02}/{}",
            timestamp_local_date.day(),
            timestamp_local_date.month() as u32,
            timestamp_local_date.year()
        ),
    }
}

pub fn format_timestamp_naive(
    timestamp_local: OffsetDateTime,
    reference_local: OffsetDateTime,
    is_12_hour_time: bool,
) -> String {
    let formatted_time = format_timestamp_naive_time(timestamp_local, is_12_hour_time);
    let reference_local_date = reference_local.date();
    let timestamp_local_date = timestamp_local.date();

    if timestamp_local_date == reference_local_date {
        format!("Today at {}", formatted_time)
    } else if reference_local_date.previous_day() == Some(timestamp_local_date) {
        format!("Yesterday at {}", formatted_time)
    } else {
        let formatted_date = match is_12_hour_time {
            true => format!(
                "{:02}/{:02}/{}",
                timestamp_local_date.month() as u32,
                timestamp_local_date.day(),
                timestamp_local_date.year()
            ),
            false => format!(
                "{:02}/{:02}/{}",
                timestamp_local_date.day(),
                timestamp_local_date.month() as u32,
                timestamp_local_date.year()
            ),
        };
        format!("{} {}", formatted_date, formatted_time)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
static CURRENT_LOCALE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn format_timestamp_fallback(timestamp: OffsetDateTime, reference: OffsetDateTime) -> String {
    let current_locale = CURRENT_LOCALE
        .get_or_init(|| sys_locale::get_locale().unwrap_or_else(|| String::from("en-US")));

    let is_12_hour_time = is_12_hour_time_by_locale(current_locale.as_str());
    format_timestamp_naive(timestamp, reference, is_12_hour_time)
}

/// Returns `true` if the locale is recognized as a 12-hour time locale.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn is_12_hour_time_by_locale(locale: &str) -> bool {
    [
        "es-MX", "es-CO", "es-SV", "es-NI",
        "es-HN", // Mexico, Colombia, El Salvador, Nicaragua, Honduras
        "en-US", "en-CA", "en-AU", "en-NZ", // U.S, Canada, Australia, New Zealand
        "ar-SA", "ar-EG", "ar-JO", // Saudi Arabia, Egypt, Jordan
        "en-IN", "hi-IN", // India, Hindu
        "en-PK", "ur-PK", // Pakistan, Urdu
        "en-PH", "fil-PH", // Philippines, Filipino
        "bn-BD", "ccp-BD", // Bangladesh, Chakma
        "en-IE", "ga-IE", // Ireland, Irish
        "en-MY", "ms-MY", // Malaysia, Malay
    ]
    .contains(&locale)
}

#[cfg(target_os = "macos")]
mod macos {
    use core_foundation::base::TCFType;
    use core_foundation::date::CFAbsoluteTime;
    use core_foundation::string::CFString;
    use core_foundation_sys::date_formatter::CFDateFormatterCreateStringWithAbsoluteTime;
    use core_foundation_sys::date_formatter::CFDateFormatterRef;
    use core_foundation_sys::locale::CFLocaleRef;
    use core_foundation_sys::{
        base::kCFAllocatorDefault,
        date_formatter::{
            CFDateFormatterCreate, kCFDateFormatterMediumStyle, kCFDateFormatterNoStyle,
            kCFDateFormatterShortStyle,
        },
        locale::CFLocaleCopyCurrent,
    };

    pub fn format_time(timestamp: &time::OffsetDateTime) -> String {
        format_with_date_formatter(timestamp, TIME_FORMATTER.with(|f| *f))
    }

    pub fn format_date(timestamp: &time::OffsetDateTime) -> String {
        format_with_date_formatter(timestamp, DATE_FORMATTER.with(|f| *f))
    }

    pub fn format_date_medium(timestamp: &time::OffsetDateTime) -> String {
        format_with_date_formatter(timestamp, MEDIUM_DATE_FORMATTER.with(|f| *f))
    }

    fn format_with_date_formatter(
        timestamp: &time::OffsetDateTime,
        fmt: CFDateFormatterRef,
    ) -> String {
        const UNIX_TO_CF_ABSOLUTE_TIME_OFFSET: i64 = 978307200;
        // Convert timestamp to macOS absolute time
        let timestamp_macos = timestamp.unix_timestamp() - UNIX_TO_CF_ABSOLUTE_TIME_OFFSET;
        let cf_absolute_time = timestamp_macos as CFAbsoluteTime;
        unsafe {
            let s = CFDateFormatterCreateStringWithAbsoluteTime(
                kCFAllocatorDefault,
                fmt,
                cf_absolute_time,
            );
            CFString::wrap_under_create_rule(s).to_string()
        }
    }

    thread_local! {
        static CURRENT_LOCALE: CFLocaleRef = unsafe { CFLocaleCopyCurrent() };
        static TIME_FORMATTER: CFDateFormatterRef = unsafe {
            CFDateFormatterCreate(
                kCFAllocatorDefault,
                CURRENT_LOCALE.with(|locale| *locale),
                kCFDateFormatterNoStyle,
                kCFDateFormatterShortStyle,
            )
        };
        static DATE_FORMATTER: CFDateFormatterRef = unsafe {
            CFDateFormatterCreate(
                kCFAllocatorDefault,
                CURRENT_LOCALE.with(|locale| *locale),
                kCFDateFormatterShortStyle,
                kCFDateFormatterNoStyle,
            )
        };

        static MEDIUM_DATE_FORMATTER: CFDateFormatterRef = unsafe {
            CFDateFormatterCreate(
                kCFAllocatorDefault,
                CURRENT_LOCALE.with(|locale| *locale),
                kCFDateFormatterMediumStyle,
                kCFDateFormatterNoStyle,
            )
        };
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use windows::Globalization::DateTimeFormatting::DateTimeFormatter;

    pub fn format_time(timestamp: &time::OffsetDateTime) -> String {
        format_with_formatter(DateTimeFormatter::ShortTime(), timestamp, true)
    }

    pub fn format_date(timestamp: &time::OffsetDateTime) -> String {
        format_with_formatter(DateTimeFormatter::ShortDate(), timestamp, false)
    }

    pub fn format_date_medium(timestamp: &time::OffsetDateTime) -> String {
        format_with_formatter(
            DateTimeFormatter::CreateDateTimeFormatter(windows::core::h!(
                "month.abbreviated day year.full"
            )),
            timestamp,
            false,
        )
    }

    fn format_with_formatter(
        formatter: windows::core::Result<DateTimeFormatter>,
        timestamp: &time::OffsetDateTime,
        is_time: bool,
    ) -> String {
        formatter
            .and_then(|formatter| formatter.Format(to_winrt_datetime(timestamp)))
            .map(|hstring| hstring.to_string())
            .unwrap_or_else(|_| {
                if is_time {
                    super::format_timestamp_naive_time(*timestamp, true)
                } else {
                    super::format_timestamp_naive_date(*timestamp, *timestamp, true)
                }
            })
    }

    fn to_winrt_datetime(timestamp: &time::OffsetDateTime) -> windows::Foundation::DateTime {
        // DateTime uses 100-nanosecond intervals since January 1, 1601 (UTC).
        const WINDOWS_EPOCH: time::OffsetDateTime = time::macros::datetime!(1601-01-01 0:00 UTC);
        let duration_since_winrt_epoch = *timestamp - WINDOWS_EPOCH;
        let universal_time = duration_since_winrt_epoch.whole_nanoseconds() / 100;

        windows::Foundation::DateTime {
            UniversalTime: universal_time as i64,
        }
    }
}

