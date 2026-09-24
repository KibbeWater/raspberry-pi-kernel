// time.rs
//! Calendar dates and times from Unix time (seconds since 1970-01-01 00:00 UTC), in UTC.

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
