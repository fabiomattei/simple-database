//! Minimal syntax highlighting for the `.dbs` DSL and SQL.

use eframe::egui::{self, Color32, FontId, TextFormat, text::LayoutJob};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Dbs,
    Sql,
}

const DBS_KEYWORDS: &[&str] = &[
    "table", "pk", "primary", "key", "autoincrement", "auto_increment", "unique", "not", "null",
    "nullable", "default", "check", "references", "ref", "on", "delete", "update", "index",
    "cascade", "set_null", "set_default", "restrict", "no_action", "set", "no", "action", "now",
    "true", "false",
];
const DBS_TYPES: &[&str] = &[
    "int", "integer", "bigint", "smallint", "real", "float", "double", "decimal", "numeric", "bool",
    "boolean", "text", "string", "varchar", "char", "date", "time", "timestamp", "datetime", "blob",
    "bytes", "uuid", "json", "jsonb",
];
const SQL_KEYWORDS: &[&str] = &[
    "select", "from", "where", "and", "or", "not", "null", "is", "in", "like", "between", "join",
    "left", "right", "inner", "outer", "cross", "on", "using", "group", "by", "order", "having",
    "limit", "offset", "asc", "desc", "distinct", "as", "insert", "into", "values", "update", "set",
    "delete", "create", "table", "index", "unique", "primary", "key", "foreign", "references",
    "constraint", "default", "check", "drop", "alter", "add", "column", "rename", "to", "begin",
    "commit", "rollback", "pragma", "union", "all", "case", "when", "then", "else", "end",
    "exists", "with", "cascade", "restrict", "autoincrement", "generated", "identity", "always",
    "engine", "modify", "type", "true", "false", "if", "returning", "explain", "query", "plan",
];

struct Palette {
    plain: Color32,
    keyword: Color32,
    ty: Color32,
    string: Color32,
    number: Color32,
    comment: Color32,
    punct: Color32,
}

fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            plain: Color32::from_rgb(0xd4, 0xd4, 0xd4),
            keyword: Color32::from_rgb(0x56, 0x9c, 0xd6),
            ty: Color32::from_rgb(0x4e, 0xc9, 0xb0),
            string: Color32::from_rgb(0xce, 0x91, 0x78),
            number: Color32::from_rgb(0xb5, 0xce, 0xa8),
            comment: Color32::from_rgb(0x6a, 0x99, 0x55),
            punct: Color32::from_rgb(0xc5, 0x86, 0xc0),
        }
    } else {
        Palette {
            plain: Color32::from_rgb(0x1f, 0x1f, 0x1f),
            keyword: Color32::from_rgb(0x00, 0x00, 0xc0),
            ty: Color32::from_rgb(0x26, 0x7f, 0x99),
            string: Color32::from_rgb(0xa3, 0x15, 0x15),
            number: Color32::from_rgb(0x09, 0x86, 0x58),
            comment: Color32::from_rgb(0x00, 0x80, 0x00),
            punct: Color32::from_rgb(0xaf, 0x00, 0xdb),
        }
    }
}

pub fn highlight(ui: &egui::Ui, text: &str, lang: Lang) -> LayoutJob {
    let p = palette(ui.visuals().dark_mode);
    let font = FontId::monospace(13.5);
    let mut job = LayoutJob::default();
    let mut push = |s: &str, color: Color32| {
        job.append(s, 0.0, TextFormat::simple(font.clone(), color));
    };

    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap();

        let comment_start = match lang {
            Lang::Dbs => rest.starts_with('#') || rest.starts_with("//"),
            Lang::Sql => rest.starts_with("--"),
        };
        if comment_start {
            let end = rest.find('\n').unwrap_or(rest.len());
            push(&rest[..end], p.comment);
            i += end;
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            let mut end = 1;
            while end < rest.len() {
                if bytes[i + end] == c as u8 {
                    end += 1;
                    if bytes.get(i + end) == Some(&(c as u8)) {
                        end += 1; // doubled quote escape
                        continue;
                    }
                    break;
                }
                end += rest[end..].chars().next().map_or(1, char::len_utf8);
            }
            let color = if c == '\'' { p.string } else { p.plain };
            push(&rest[..end], color);
            i += end;
            continue;
        }
        if c.is_ascii_digit() {
            let end = rest
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.'))
                .unwrap_or(rest.len());
            push(&rest[..end], p.number);
            i += end;
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let end = rest
                .find(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '.'))
                .unwrap_or(rest.len());
            let word = &rest[..end];
            let lower = word.to_ascii_lowercase();
            let color = match lang {
                Lang::Dbs if DBS_KEYWORDS.contains(&lower.as_str()) => p.keyword,
                Lang::Dbs if DBS_TYPES.contains(&lower.as_str()) => p.ty,
                Lang::Sql if SQL_KEYWORDS.contains(&lower.as_str()) => p.keyword,
                Lang::Sql if DBS_TYPES.contains(&lower.as_str()) => p.ty,
                _ => p.plain,
            };
            push(word, color);
            i += end;
            continue;
        }
        if rest.starts_with("->") {
            push("->", p.punct);
            i += 2;
            continue;
        }
        let len = c.len_utf8();
        let color = if "{}();,".contains(c) { p.punct } else { p.plain };
        push(&rest[..len], color);
        i += len;
    }
    job
}

/// A monospace code editor with highlighting. Returns the response of the text edit.
pub fn code_editor(ui: &mut egui::Ui, text: &mut String, lang: Lang, id: &str, rows: usize) -> egui::Response {
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
        let mut job = highlight(ui, buf.as_str(), lang);
        job.wrap.max_width = wrap_width;
        ui.fonts_mut(|f| f.layout_job(job))
    };
    ui.add(
        egui::TextEdit::multiline(text)
            .id_salt(id)
            .code_editor()
            .desired_rows(rows)
            .desired_width(f32::INFINITY)
            .lock_focus(true)
            .layouter(&mut layouter),
    )
}

/// Read-only highlighted code, selectable for copying. Never wraps or
/// justifies, even inside a justified layout such as `ui.columns`.
pub fn code_view(ui: &mut egui::Ui, text: &str, lang: Lang) {
    let job = highlight(ui, text, lang);
    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.add(egui::Label::new(job).selectable(true).wrap_mode(egui::TextWrapMode::Extend));
    });
}
