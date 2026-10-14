//! Minimal syntax highlighting for the `.dbs` DSL and SQL.

use eframe::egui::{self, Color32, FontId, TextFormat, text::LayoutJob};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Dbs,
    Sql,
    /// Laravel migrations.
    Php,
    /// Rails migrations.
    Ruby,
}

const PHP_KEYWORDS: &[&str] = &["use", "return", "new", "class", "extends", "function", "public", "void", "if", "throw", "true", "false", "null"];
const RUBY_KEYWORDS: &[&str] = &["class", "def", "end", "do", "if", "ensure", "raise", "true", "false", "nil"];

pub const DBS_KEYWORDS: &[&str] = &[
    "table", "pk", "primary", "key", "autoincrement", "auto_increment", "unique", "not", "null",
    "nullable", "default", "check", "references", "ref", "on", "delete", "update", "index",
    "cascade", "set_null", "set_default", "restrict", "no_action", "set", "no", "action", "now",
    "true", "false", "was", "fk", "foreign", "comment",
];
pub const DBS_TYPES: &[&str] = &[
    "int", "integer", "bigint", "smallint", "real", "float", "double", "decimal", "numeric", "bool",
    "boolean", "text", "string", "varchar", "char", "date", "time", "timestamp", "datetime", "blob",
    "bytes", "uuid", "json", "jsonb", "enum",
];
pub const SQL_KEYWORDS: &[&str] = &[
    "select", "from", "where", "and", "or", "not", "null", "is", "in", "like", "between", "join",
    "left", "right", "inner", "outer", "cross", "on", "using", "group", "by", "order", "having",
    "limit", "offset", "asc", "desc", "distinct", "as", "insert", "into", "values", "update", "set",
    "delete", "create", "table", "index", "unique", "primary", "key", "foreign", "references",
    "constraint", "default", "check", "drop", "alter", "add", "column", "rename", "to", "begin",
    "commit", "rollback", "pragma", "union", "all", "case", "when", "then", "else", "end",
    "exists", "with", "cascade", "restrict", "autoincrement", "generated", "identity", "always",
    "engine", "modify", "type", "true", "false", "if", "returning", "explain", "query", "plan",
    "comment", "enum",
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

/// `text` highlighted as `lang`. Kept from frame to frame while the text,
/// language and theme stay the same: an editor lays its text out every frame.
pub fn highlight(ui: &egui::Ui, text: &str, lang: Lang) -> Arc<LayoutJob> {
    let dark = ui.visuals().dark_mode;
    ui.ctx().memory_mut(|m| m.caches.cache::<HighlightCache>().get((text, lang, dark)).clone())
}

type HighlightCache = egui::cache::FrameCache<Arc<LayoutJob>, Highlighter>;

#[derive(Default)]
struct Highlighter;

impl egui::cache::ComputerMut<(&str, Lang, bool), Arc<LayoutJob>> for Highlighter {
    fn compute(&mut self, (text, lang, dark): (&str, Lang, bool)) -> Arc<LayoutJob> {
        Arc::new(highlight_job(text, lang, dark))
    }
}

fn highlight_job(text: &str, lang: Lang, dark: bool) -> LayoutJob {
    let p = palette(dark);
    let font = FontId::monospace(13.5);
    let mut job = LayoutJob::default();
    // Neighbors of one color share a section: fewer for egui to lay out.
    let mut push = |s: &str, color: Color32| match job.sections.last_mut() {
        Some(last) if last.format.color == color => {
            job.text.push_str(s);
            last.byte_range.end = egui::text::ByteIndex(job.text.len());
        }
        _ => job.append(s, 0.0, TextFormat::simple(font.clone(), color)),
    };

    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap();

        let comment_start = match lang {
            Lang::Dbs => rest.starts_with('#') || rest.starts_with("//"),
            Lang::Sql => rest.starts_with("--"),
            Lang::Php => rest.starts_with("//"),
            Lang::Ruby => rest.starts_with('#'),
        };
        if comment_start {
            let end = rest.find('\n').unwrap_or(rest.len());
            push(&rest[..end], p.comment);
            i += end;
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            let mut end = 1;
            let escapes = matches!(lang, Lang::Php | Lang::Ruby);
            while end < rest.len() {
                if escapes && bytes[i + end] == b'\\' {
                    end += 1 + rest[end + 1..].chars().next().map_or(0, char::len_utf8);
                    continue;
                }
                if bytes[i + end] == c as u8 {
                    end += 1;
                    if !escapes && bytes.get(i + end) == Some(&(c as u8)) {
                        end += 1; // doubled quote escape
                        continue;
                    }
                    break;
                }
                end += rest[end..].chars().next().map_or(1, char::len_utf8);
            }
            let color = if c == '\'' || (c == '"' && matches!(lang, Lang::Php | Lang::Ruby)) { p.string } else { p.plain };
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
                Lang::Php if PHP_KEYWORDS.contains(&word) => p.keyword,
                Lang::Ruby if RUBY_KEYWORDS.contains(&word) => p.keyword,
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

/// Read-only highlighted code, selectable for copying. Never wraps or
/// justifies, even inside a justified layout such as `ui.columns`.
pub fn code_view(ui: &mut egui::Ui, text: &str, lang: Lang) {
    let job = highlight(ui, text, lang);
    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.add(egui::Label::new(job).selectable(true).wrap_mode(egui::TextWrapMode::Extend));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neighbors_of_one_color_share_a_section() {
        let text = "select a, b from t where x = 'it''s' -- done\nupdate t set y = 1;\n";
        let job = highlight_job(text, Lang::Sql, true);
        assert_eq!(job.text, text);
        let p = palette(true);
        let parts: Vec<(&str, Color32)> = job.sections.iter().map(|s| (&job.text[s.byte_range.start.0..s.byte_range.end.0], s.format.color)).collect();
        assert!(parts.windows(2).all(|w| w[0].1 != w[1].1), "{parts:?}");
        assert_eq!(parts.iter().map(|p| p.0).collect::<String>(), text);
        assert!(parts.contains(&(" a", p.plain)) && parts.contains(&("'it''s'", p.string)) && parts.contains(&("-- done", p.comment)));
    }

    #[test]
    fn highlighting_is_kept_while_the_text_stays() {
        let ctx = egui::Context::default();
        let frame = |text: &str, lang: Lang| {
            let mut job = None;
            let mut out = ctx.run_ui(Default::default(), |ui| job = Some(highlight(ui, text, lang)));
            out.textures_delta.clear();
            job.unwrap()
        };
        let first = frame("table t { id int pk }", Lang::Dbs);
        assert!(Arc::ptr_eq(&first, &frame("table t { id int pk }", Lang::Dbs)));
        assert!(!Arc::ptr_eq(&first, &frame("table t { id int pk }", Lang::Sql)));
        assert!(!Arc::ptr_eq(&first, &frame("table t { id bigint pk }", Lang::Dbs)));
    }
}
