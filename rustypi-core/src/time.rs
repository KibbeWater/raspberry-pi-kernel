// time.rs
//! Calendar dates and times from Unix time (seconds since 1970-01-01 00:00 UTC), in UTC or
//! a time zone (`Zone`).

use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub year: i64,
    /// 1 to 12.
    pub month: u8,
    /// 1 to 31.
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// 0 for Monday to 6 for Sunday.
    pub weekday: u8,
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// Days from 1970-01-01 to `year`-`month`-`day` (Howard Hinnant's days_from_civil). `month`
/// may be 13, meaning January of the next year.
fn days_from_civil(year: i64, month: u8, day: u8) -> i64 {
    let (year, month) = if month > 12 { (year + 1, month - 12) } else { (year, month) };
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The day (counted from 1970-01-01) of the last Sunday in `year`'s `month`.
fn last_sunday(year: i64, month: u8) -> i64 {
    let last = days_from_civil(year, month + 1, 1) - 1;
    // 1970-01-01 was a Thursday: day d is weekday (d + 3) mod 7, with Sunday 6.
    last - ((last + 3).rem_euclid(7) + 1) % 7
}

/// A time zone: how far local time is from UTC, and when that changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    Utc,
    /// Sweden (and most of central Europe): CET, UTC+1, and CEST, UTC+2, from the last
    /// Sunday of March to the last Sunday of October, switching at 01:00 UTC.
    Sweden,
}

impl Zone {
    pub const ALL: [Zone; 2] = [Zone::Utc, Zone::Sweden];

    /// Its short name, as `from_name` takes it.
    pub fn name(self) -> &'static str {
        match self {
            Zone::Utc => "utc",
            Zone::Sweden => "sv",
        }
    }

    pub fn from_name(name: &str) -> Option<Zone> {
        match name {
            "utc" => Some(Zone::Utc),
            "sv" | "se" | "cet" => Some(Zone::Sweden),
            _ => None,
        }
    }

    /// How many seconds local time is ahead of UTC at `unix`, and what that time is called.
    pub fn offset(self, unix: u64) -> (u64, &'static str) {
        match self {
            Zone::Utc => (0, "UTC"),
            Zone::Sweden => {
                let year = DateTime::from_unix(unix).year;
                let start = last_sunday(year, 3) * 86_400 + 3600;
                let end = last_sunday(year, 10) * 86_400 + 3600;
                if (start..end).contains(&(unix as i64)) { (7200, "CEST") } else { (3600, "CET") }
            }
        }
    }

    /// The local date and time at `unix`, and what the time is called.
    pub fn local(self, unix: u64) -> (DateTime, &'static str) {
        let (offset, name) = self.offset(unix);
        (DateTime::from_unix(unix + offset), name)
    }
}

impl DateTime {
    /// The date and time `unix` seconds after 1970-01-01 00:00 UTC.
    pub fn from_unix(unix: u64) -> Self {
        let days = (unix / 86_400) as i64;
        let seconds = unix % 86_400;
        // Howard Hinnant's civil_from_days: eras of 400 years, counted from March so the leap
        // day comes last.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
        let year = yoe + era * 400 + (month <= 2) as i64;
        DateTime {
            year,
            month,
            day,
            hour: (seconds / 3600) as u8,
            minute: (seconds / 60 % 60) as u8,
            second: (seconds % 60) as u8,
            // 1970-01-01 was a Thursday.
            weekday: (days + 3).rem_euclid(7) as u8,
        }
    }

    /// As a FAT directory entry has it: the date (years since 1980, month, day) and the time
    /// (hours, minutes, seconds in twos). Dates outside FAT's 1980 to 2107 are clamped.
    pub fn fat(&self) -> (u16, u16) {
        if self.year < 1980 {
            return (1 << 5 | 1, 0);
        }
        let year = (self.year - 1980).min(127) as u16;
        let date = year << 9 | (self.month as u16) << 5 | self.day as u16;
        let time = (self.hour as u16) << 11 | (self.minute as u16) << 5 | ((self.second as u16) / 2);
        (date, time)
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} {}-{:02}-{:02} {:02}:{:02}:{:02}",
            WEEKDAYS[self.weekday as usize], self.year, self.month, self.day, self.hour, self.minute, self.second,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn unix_times_become_dates() {
        assert_eq!(format!("{}", DateTime::from_unix(0)), "Thu 1970-01-01 00:00:00");
        assert_eq!(format!("{}", DateTime::from_unix(1_000_000_000)), "Sun 2001-09-09 01:46:40");
        // A leap day, and the day after.
        assert_eq!(format!("{}", DateTime::from_unix(1_709_164_800)), "Thu 2024-02-29 00:00:00");
        assert_eq!(format!("{}", DateTime::from_unix(1_709_251_199)), "Thu 2024-02-29 23:59:59");
        assert_eq!(format!("{}", DateTime::from_unix(1_709_251_200)), "Fri 2024-03-01 00:00:00");
        // Century rules: 2100 isn't a leap year.
        assert_eq!(format!("{}", DateTime::from_unix(4_107_542_400)), "Mon 2100-03-01 00:00:00");
    }

    #[test]
    fn days_count_both_ways() {
        for unix in [0, 951_782_400, 1_709_164_800, 4_107_542_400u64] {
            let date = DateTime::from_unix(unix);
            assert_eq!(days_from_civil(date.year, date.month, date.day) * 86_400, unix as i64);
        }
        assert_eq!(days_from_civil(2025, 13, 1), days_from_civil(2026, 1, 1));
    }

    #[test]
    fn swedish_summer_time_starts_and_ends_on_the_last_sundays() {
        // 2026: from Sunday 29 March to Sunday 25 October, at 01:00 UTC.
        let start = 1_774_746_000; // 2026-03-29 01:00:00 UTC
        let end = 1_792_890_000; // 2026-10-25 01:00:00 UTC
        assert_eq!(format!("{}", DateTime::from_unix(start)), "Sun 2026-03-29 01:00:00");
        assert_eq!(format!("{}", DateTime::from_unix(end)), "Sun 2026-10-25 01:00:00");
        assert_eq!(Zone::Sweden.offset(start - 1), (3600, "CET"));
        assert_eq!(Zone::Sweden.offset(start), (7200, "CEST"));
        assert_eq!(Zone::Sweden.offset(end - 1), (7200, "CEST"));
        assert_eq!(Zone::Sweden.offset(end), (3600, "CET"));
        // Local clocks jump from 02:00 to 03:00 in March, and back from 03:00 to 02:00.
        assert_eq!(format!("{}", Zone::Sweden.local(start - 1).0), "Sun 2026-03-29 01:59:59");
        assert_eq!(format!("{}", Zone::Sweden.local(start).0), "Sun 2026-03-29 03:00:00");
        assert_eq!(format!("{}", Zone::Sweden.local(end - 1).0), "Sun 2026-10-25 02:59:59");
        assert_eq!(format!("{}", Zone::Sweden.local(end).0), "Sun 2026-10-25 02:00:00");
        // A leap year, and across midnight.
        assert_eq!(Zone::Sweden.local(1_711_846_800).1, "CEST"); // 2024-03-31 01:00 UTC
        assert_eq!(format!("{}", Zone::Sweden.local(1_790_029_800).0), "Tue 2026-09-22 00:30:00"); // 22:30 UTC
        assert_eq!(Zone::Utc.local(1_790_020_000), (DateTime::from_unix(1_790_020_000), "UTC"));
    }

    #[test]
    fn zones_are_chosen_by_name() {
        for zone in Zone::ALL {
            assert_eq!(Zone::from_name(zone.name()), Some(zone));
        }
        assert_eq!(Zone::from_name("mars"), None);
    }

    #[test]
    fn dates_pack_into_fat_fields() {
        let date = DateTime::from_unix(1_790_000_000); // 2026-09-21 14:13:20
        assert_eq!(format!("{date}"), "Mon 2026-09-21 14:13:20");
        let (fat_date, fat_time) = date.fat();
        assert_eq!(fat_date, (46 << 9) | (9 << 5) | 21);
        assert_eq!(fat_time, (14 << 11) | (13 << 5) | 10);
        // Before FAT's epoch: its first day.
        assert_eq!(DateTime::from_unix(0).fat(), (1 << 5 | 1, 0));
    }
}
