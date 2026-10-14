//! Pickers for date, time and timestamp cells in the Data tab. They edit the
//! cell's text in place: only its `YYYY-MM-DD` and `HH:MM:SS` parts change,
//! anything after them (fractional seconds, a time zone) is kept.

use eframe::egui::{self, RichText};
use simple_database::schema::DataType;
use std::time::{SystemTime, UNIX_EPOCH};

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Date,
    Time,
    Timestamp,
}

impl Kind {
    pub fn of(ty: &DataType) -> Option<Kind> {
        match ty {
            DataType::Date => Some(Kind::Date),
            DataType::Time => Some(Kind::Time),
            DataType::Timestamp => Some(Kind::Timestamp),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Date {
    year: i32,
    month: u32,
    day: u32,
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
struct Time {
    hour: u32,
    minute: u32,
    second: u32,
}

/// A cell's text split into the parts a picker edits and what follows them.
#[derive(Clone, PartialEq, Debug)]
struct Parts {
    date: Date,
    /// Between date and time in a timestamp: a space or `T`.
    sep: char,
    time: Time,
    rest: String,
}

/// What the picker means for the text field it sits beside.
pub struct Picker {
    /// Open, or being clicked: the field must not commit because it lost focus.
    pub busy: bool,
    /// Closed this frame: the field takes the focus back.
    pub closed: bool,
}

/// A button that opens a calendar and/or clock editing `text`.
pub fn picker(ui: &mut egui::Ui, kind: Kind, text: &mut String) -> Picker {
    let (icon, hint) = match kind {
        Kind::Date => ("📅", "Pick a date"),
        Kind::Time => ("🕒", "Pick a time"),
        Kind::Timestamp => ("📅", "Pick a date and time"),
    };
    let button = ui.add(egui::Button::new(icon).small()).on_hover_text(hint);
    let popup_id = egui::Popup::default_response_id(&button);
    let was_open = egui::Popup::is_id_open(ui.ctx(), popup_id);
    if button.clicked() && !was_open {
        // Start the calendar at the cell's month, not the one browsed last time.
        ui.data_mut(|d| d.remove_temp::<(i32, u32)>(popup_id.with("month")));
    }
    egui::Popup::from_toggle_button_response(&button)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            let today = today();
            let mut parts = split(kind, text, today);
            let mut changed = false;
            if kind != Kind::Time {
                changed |= calendar(ui, popup_id, &mut parts.date, today);
            }
            if kind != Kind::Date {
                changed |= clock(ui, &mut parts.time);
            }
            if changed {
                *text = join(kind, &parts);
            }
        });
    let open = egui::Popup::is_id_open(ui.ctx(), popup_id);
    Picker { busy: was_open || open || button.hovered(), closed: was_open && !open }
}

/// A month of days; returns whether a day was picked.
fn calendar(ui: &mut egui::Ui, id: egui::Id, date: &mut Date, today: Date) -> bool {
    let key = id.with("month");
    let (mut year, mut month) = ui.data(|d| d.get_temp::<(i32, u32)>(key)).unwrap_or((date.year, date.month));
    let mut picked = false;
    ui.horizontal(|ui| {
        if ui.small_button("‹").on_hover_text("Previous month").clicked() {
            (year, month) = if month == 1 { (year - 1, 12) } else { (year, month - 1) };
        }
        ui.label(RichText::new(format!("{} {year}", MONTHS[month as usize - 1])).strong());
        if ui.small_button("›").on_hover_text("Next month").clicked() {
            (year, month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
        }
        if ui.small_button("Today").clicked() {
            *date = today;
            (year, month) = (today.year, today.month);
            picked = true;
        }
    });
    egui::Grid::new(id.with("days")).spacing([2.0, 2.0]).show(ui, |ui| {
        for name in ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"] {
            ui.label(RichText::new(name).small().weak());
        }
        ui.end_row();
        let first = weekday(Date { year, month, day: 1 });
        for _ in 0..first {
            ui.label("");
        }
        for day in 1..=days_in_month(year, month) {
            let this = Date { year, month, day };
            let mut label = RichText::new(format!("{day:>2}")).monospace();
            if this == today {
                label = label.strong();
            }
            if ui.selectable_label(this == *date, label).clicked() {
                *date = this;
                picked = true;
            }
            if (first + day).is_multiple_of(7) {
                ui.end_row();
            }
        }
    });
    ui.data_mut(|d| d.insert_temp(key, (year, month)));
    picked
}

/// Hour, minute and second spinners; returns whether one changed.
fn clock(ui: &mut egui::Ui, time: &mut Time) -> bool {
    ui.horizontal(|ui| {
        let mut changed = false;
        for (i, (value, max)) in [(&mut time.hour, 23), (&mut time.minute, 59), (&mut time.second, 59)].into_iter().enumerate() {
            if i > 0 {
                ui.label(":");
            }
            let drag = egui::DragValue::new(value).range(0..=max).custom_formatter(|n, _| format!("{n:02}"));
            changed |= ui.add(drag).changed();
        }
        changed
    })
    .inner
}

/// The parts of `text`; what doesn't parse starts from `today` and midnight.
/// The rest is kept only after a part that parsed, so junk is replaced.
fn split(kind: Kind, text: &str, today: Date) -> Parts {
    let mut parts = Parts { date: today, sep: ' ', time: Time::default(), rest: String::new() };
    let mut s = text.trim();
    let parsed = match kind {
        Kind::Date | Kind::Timestamp => parse_date(s).inspect(|d| {
            parts.date = *d;
            s = &s[10..];
        }),
        Kind::Time => None,
    };
    if kind == Kind::Timestamp
        && parsed.is_some()
        && let Some(sep) = s.chars().next().filter(|c| *c == ' ' || *c == 'T')
        && let Some((time, n)) = parse_time(&s[1..])
    {
        (parts.sep, parts.time) = (sep, time);
        s = &s[1 + n..];
    }
    let parsed = match kind {
        Kind::Time => parse_time(s).map(|(time, n)| {
            parts.time = time;
            s = &s[n..];
        }),
        _ => parsed.map(|_| ()),
    };
    if parsed.is_some() {
        parts.rest = s.to_string();
    }
    parts
}

fn join(kind: Kind, p: &Parts) -> String {
    let date = format!("{:04}-{:02}-{:02}", p.date.year, p.date.month, p.date.day);
    let time = format!("{:02}:{:02}:{:02}", p.time.hour, p.time.minute, p.time.second);
    match kind {
        Kind::Date => format!("{date}{}", p.rest),
        Kind::Time => format!("{time}{}", p.rest),
        Kind::Timestamp => format!("{date}{}{time}{}", p.sep, p.rest),
    }
}

/// `YYYY-MM-DD` at the start of `s`, if it is a real day.
fn parse_date(s: &str) -> Option<Date> {
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year = digits(&b[0..4])? as i32;
    let (month, day) = (digits(&b[5..7])?, digits(&b[8..10])?);
    ((1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day)).then_some(Date { year, month, day })
}

/// `HH:MM` or `HH:MM:SS` at the start of `s`, and its length.
fn parse_time(s: &str) -> Option<(Time, usize)> {
    let b = s.as_bytes();
    if b.len() < 5 || b[2] != b':' {
        return None;
    }
    let (hour, minute) = (digits(&b[0..2])?, digits(&b[3..5])?);
    let (second, n) = match b.get(5..8) {
        Some([b':', ..]) => (digits(&b[6..8])?, 8),
        _ => (0, 5),
    };
    (hour < 24 && minute < 60 && second < 60).then_some((Time { hour, minute, second }, n))
}

fn digits(b: &[u8]) -> Option<u32> {
    b.iter().try_fold(0, |n, c| c.is_ascii_digit().then(|| n * 10 + u32::from(c - b'0')))
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Monday = 0 … Sunday = 6.
fn weekday(date: Date) -> u32 {
    // 1970-01-01 was a Thursday.
    (days_from_civil(date) + 3).rem_euclid(7) as u32
}

/// Today in UTC: the standard library has no local time zone.
fn today() -> Date {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    civil_from_days((secs / 86_400) as i64)
}

/// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
fn days_from_civil(date: Date) -> i64 {
    let (m, d) = (i64::from(date.month), i64::from(date.day));
    let y = i64::from(date.year) - i64::from(m <= 2);
    let (era, yoe) = (y.div_euclid(400), y.rem_euclid(400));
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date `days` after 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> Date {
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    Date { year: (yoe + era * 400 + i64::from(month <= 2)) as i32, month, day }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TODAY: Date = Date { year: 2026, month: 10, day: 14 };

    fn edit(kind: Kind, text: &str, change: impl FnOnce(&mut Parts)) -> String {
        let mut parts = split(kind, text, TODAY);
        change(&mut parts);
        join(kind, &parts)
    }

    #[test]
    fn edits_keep_what_follows_the_parts() {
        assert_eq!(edit(Kind::Timestamp, "2024-03-05T10:20:30.123+02", |p| p.date.day = 6), "2024-03-06T10:20:30.123+02");
        assert_eq!(edit(Kind::Timestamp, "2024-03-05 10:20", |p| p.time.second = 9), "2024-03-05 10:20:09");
        assert_eq!(edit(Kind::Time, "10:20:30.5", |p| p.time.hour = 11), "11:20:30.5");
        assert_eq!(edit(Kind::Date, "2024-03-05", |p| p.date.month = 4), "2024-04-05");
    }

    #[test]
    fn unparsed_text_starts_from_today() {
        assert_eq!(edit(Kind::Timestamp, "", |_| ()), "2026-10-14 00:00:00");
        assert_eq!(edit(Kind::Date, "soon", |_| ()), "2026-10-14");
        assert_eq!(edit(Kind::Time, "25:00", |_| ()), "00:00:00");
        assert_eq!(edit(Kind::Timestamp, "2024-03-05", |_| ()), "2024-03-05 00:00:00");
    }

    #[test]
    fn rejects_days_that_do_not_exist() {
        assert_eq!(parse_date("2023-02-29"), None);
        assert!(parse_date("2024-02-29").is_some());
        assert_eq!(parse_date("2024-13-01"), None);
    }

    #[test]
    fn calendar_arithmetic() {
        assert_eq!(days_in_month(1900, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29);
        assert_eq!(weekday(Date { year: 1970, month: 1, day: 1 }), 3);
        assert_eq!(weekday(Date { year: 2024, month: 1, day: 1 }), 0);
        assert_eq!(weekday(Date { year: 2026, month: 10, day: 18 }), 6);
        assert_eq!(civil_from_days(0), Date { year: 1970, month: 1, day: 1 });
        let leap = Date { year: 2000, month: 2, day: 29 };
        assert_eq!(civil_from_days(days_from_civil(leap)), leap);
    }
}
