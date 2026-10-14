//! Pretty-print a schema back to the `.dbs` DSL.

use crate::parser::parse;
use crate::schema::*;

pub fn format_schema(schema: &Schema) -> String {
    schema
        .tables
        .iter()
        .map(format_table)
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_table(t: &Table) -> String {
    let name_w = t.columns.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let types: Vec<String> = t.columns.iter().map(|c| c.data_type.to_string()).collect();
    let type_w = types.iter().map(|s| s.len()).max().unwrap_or(0);

    let mut out = format!("table {}{} {{\n", t.name, was(&t.was));
    for (c, ty) in t.columns.iter().zip(&types) {
        let mods = modifiers(c);
        let line = format!("  {:name_w$}  {:type_w$}  {}", c.name, ty, mods.join(" "));
        out.push_str(line.trim_end());
        out.push('\n');
    }
    if !t.composite_pk.is_empty() || !t.indexes.is_empty() {
        out.push('\n');
    }
    if !t.composite_pk.is_empty() {
        out.push_str(&format!("  pk ({})\n", t.composite_pk.join(", ")));
    }
    for idx in &t.indexes {
        let default_name = format!(
            "{}_{}_{}",
            t.name,
            idx.columns.join("_"),
            if idx.unique { "key" } else { "idx" }
        );
        let name = if idx.name == default_name { String::new() } else { format!("{} ", idx.name) };
        out.push_str(&format!(
            "  {}index {name}({})\n",
            if idx.unique { "unique " } else { "" },
            idx.columns.join(", ")
        ));
    }
    out.push_str("}\n");
    out
}

fn modifiers(c: &Column) -> Vec<String> {
    let mut m = Vec::new();
    if c.primary_key {
        m.push("pk".to_string());
    }
    if c.auto_increment {
        m.push("autoincrement".to_string());
    }
    if c.unique {
        m.push("unique".to_string());
    }
    if !c.nullable && !c.primary_key {
        m.push("not null".to_string());
    }
    if let Some(d) = &c.default {
        m.push(format!("default {d}"));
    }
    if let Some(chk) = &c.check {
        m.push(format!("check ({chk})"));
    }
    if let Some(fk) = &c.references {
        m.push(format!("-> {}.{}", fk.table, fk.column));
        if fk.on_delete != RefAction::NoAction {
            m.push(format!("on delete {}", fk.on_delete.dsl()));
        }
        if fk.on_update != RefAction::NoAction {
            m.push(format!("on update {}", fk.on_update.dsl()));
        }
    }
    let was = was(&c.was);
    if !was.is_empty() {
        m.push(was.trim_start().to_string());
    }
    m
}

/// ` was a` or ` was (a, b)`; empty without earlier names.
fn was(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!(" was {one}"),
        many => format!(" was ({})", many.join(", ")),
    }
}

/// Re-aligns the column definitions of every table the way `format_schema`
/// does, keeping comments, blank lines and everything else as written.
/// `None` when `src` doesn't parse, or if the result would mean something else.
pub fn align(src: &str) -> Option<String> {
    let before = parse(src).ok()?;
    let lines: Vec<&str> = src.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        i += 1;
        let code = code_part(line.trim()).unwrap_or("").trim_end();
        let opens = code.get(..6).is_some_and(|p| p.trim_end().eq_ignore_ascii_case("table") && p.ends_with(char::is_whitespace))
            && code.ends_with('{')
            && !code.contains('}');
        let Some(close) = lines[i..].iter().position(|l| l.trim_start().starts_with('}')).filter(|_| opens) else {
            out.push(line.to_string());
            continue;
        };
        out.push(line.trim_start().to_string());
        let body = &lines[i..i + close];
        let cols: Vec<Option<(&str, String, &str)>> = body.iter().map(|l| split_column(l.trim())).collect();
        let name_w = cols.iter().flatten().map(|c| c.0.chars().count()).max().unwrap_or(0);
        let type_w = cols.iter().flatten().map(|c| c.1.chars().count()).max().unwrap_or(0);
        for (l, col) in body.iter().zip(&cols) {
            let l = match col {
                Some((name, ty, rest)) => format!("  {name:name_w$}  {ty:type_w$}  {rest}"),
                None if l.trim().is_empty() => String::new(),
                None => format!("  {}", l.trim()),
            };
            out.push(l.trim_end().to_string());
        }
        out.push(lines[i + close].trim().to_string());
        i += close + 1;
    }
    let text = out.join("\n");
    (parse(&text).ok()? == before).then_some(text)
}

/// `src` changed so that it describes `new`, rewriting only the tables that
/// differ: changed tables are reprinted in place, new ones appended, dropped
/// ones removed. Comments in untouched tables survive. Falls back to
/// `format_schema(new)` when `src` doesn't parse or its layout defeats this.
pub fn patch(src: &str, new: &Schema) -> String {
    patch_renamed(src, new, &[])
}

/// `patch`, where the tables in `renamed` (old name, new name) are reprinted
/// where the old one was instead of being dropped and appended.
pub fn patch_renamed(src: &str, new: &Schema, renamed: &[(String, String)]) -> String {
    let Ok(old) = parse(src) else { return format_schema(new) };
    // Edits that know nothing of rename hints (the SQL tab) keep the ones written.
    let mut new = new.clone();
    for t in &mut new.tables {
        let Some(o) = old.table(&t.name) else { continue };
        if t.was.is_empty() {
            t.was = o.was.clone();
        }
        for c in t.columns.iter_mut().filter(|c| c.was.is_empty()) {
            c.was = o.column(&c.name).map(|oc| oc.was.clone()).unwrap_or_default();
        }
    }
    let new = &new;
    let new_name = |old: &str| renamed.iter().find(|(o, _)| o == old).map_or(old, |(_, n)| n.as_str()).to_string();
    let outline = crate::parser::outline(src);
    let mut lines: Vec<String> = src.split('\n').map(String::from).collect();
    // Bottom-up, so earlier line numbers stay valid.
    let mut blocks: Vec<&crate::parser::OutlineTable> = outline.iter().collect();
    blocks.sort_by_key(|t| std::cmp::Reverse(t.line));
    for t in blocks {
        let (lo, hi) = (t.line - 1, t.end_line.min(lines.len()) - 1);
        match new.table(&new_name(&t.name)) {
            Some(nt) if old.table(&t.name) == Some(nt) => {}
            Some(nt) => {
                let printed = format_table(nt);
                lines.splice(lo..=hi, printed.trim_end().split('\n').map(String::from));
            }
            None => {
                let blank_after = lines.get(hi + 1).is_some_and(|l| l.trim().is_empty());
                lines.drain(lo..=hi + usize::from(blank_after));
            }
        }
    }
    let mut text = lines.join("\n");
    let kept: Vec<String> = old.tables.iter().map(|t| new_name(&t.name)).collect();
    for t in new.tables.iter().filter(|t| !kept.contains(&t.name)) {
        if !text.is_empty() && !text.ends_with("\n\n") {
            text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
        }
        text.push_str(&format_table(t));
    }
    match parse(&text) {
        Ok(s) if s.same_tables(new) => text,
        _ => format_schema(new),
    }
}

/// `line` up to its comment; `None` if it holds more than one statement or
/// leaves a quote or parenthesis open.
fn code_part(line: &str) -> Option<&str> {
    let mut depth = 0;
    let mut quote = None;
    let mut prev = ' ';
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '\'' | '"' => quote = Some(c),
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' | ';' if depth == 0 => return None,
                '#' => return (depth == 0).then_some(&line[..i]),
                '/' if prev == '/' => return (depth == 0).then_some(&line[..i - 1]),
                _ => {}
            },
        }
        prev = c;
    }
    (depth == 0 && quote.is_none()).then_some(line)
}

/// A column definition split into name, type and the rest (modifiers and comment).
fn split_column(line: &str) -> Option<(&str, String, &str)> {
    let code = code_part(line)?;
    let (name, after) = code.split_once(char::is_whitespace)?;
    if name.contains('(') || ["index", "unique", "pk", "primary"].contains(&name.to_ascii_lowercase().as_str()) {
        return None;
    }
    let after = after.trim_start();
    let offset = code.len() - after.len();
    let mut ty = String::new();
    let mut depth = 0;
    let mut end = after.len();
    for (i, c) in after.char_indices() {
        if depth == 0 && c.is_whitespace() {
            // `decimal (10, 2)`: the arguments still belong to the type.
            let next = after[i..].trim_start();
            if next.starts_with('(') && !ty.contains('(') {
                continue;
            }
            end = i;
            break;
        }
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        ty.push(c);
    }
    if ty.is_empty() {
        return None;
    }
    Some((name, ty, line[offset + end..].trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let src = "table users { id int pk autoincrement\n email varchar(255) unique not null\n \
                   role text default 'it''s' check (role <> '')\n boss_id int -> users.id on delete set null\n \
                   created timestamp default now\n index (boss_id)\n unique index by_role (role, email) }\n\
                   table tags was (labels, tag) { a int was x\n b text was (y, z)\n pk (a, b) }";
        let schema = parse(src).unwrap();
        let printed = format_schema(&schema);
        assert_eq!(parse(&printed).unwrap(), schema, "{printed}");
    }

    #[test]
    fn align_keeps_comments() {
        let src = "# header\ntable users { # people\n    id int pk autoincrement\n  # contact\n  email varchar(255) unique   not null # login\n\n  score decimal (10, 2) default 0\n    index (email)\n}\n";
        let aligned = align(src).unwrap();
        assert_eq!(
            aligned,
            "# header\ntable users { # people\n  id     int             pk autoincrement\n  # contact\n  email  varchar(255)    unique   not null # login\n\n  score  decimal(10, 2)  default 0\n  index (email)\n}\n"
        );
        assert_eq!(align(&aligned).as_deref(), Some(aligned.as_str()), "idempotent");
        assert_eq!(align("table x {").as_deref(), None, "doesn't parse");
        // One-line tables and several statements on a line stay as written.
        let src = "table a { id int pk }\ntable b {\n  x int, y int\n}\n";
        assert_eq!(align(src).as_deref(), Some(src));
    }

    #[test]
    fn align_matches_format_schema() {
        let schema = parse(crate::STARTER_SCHEMA).unwrap();
        let printed = format_schema(&schema);
        assert_eq!(align(&printed).as_deref(), Some(printed.as_str()));
    }

    #[test]
    fn patch_rewrites_only_changed_tables() {
        let src = "# header\ntable a { # keep me\n  x  int  pk\n}\n\ntable b {\n  y int\n}\n\ntable c {\n  z int\n}\n";
        let mut new = parse(src).unwrap();
        new.tables[1].columns[0].nullable = false; // b changes
        new.tables.remove(2); // c goes
        new.tables.push(parse("table d {\n  w text\n}").unwrap().tables.remove(0));
        let out = patch(src, &new);
        assert_eq!(out, "# header\ntable a { # keep me\n  x  int  pk\n}\n\ntable b {\n  y  int  not null\n}\n\ntable d {\n  w  text\n}\n");
        assert!(parse(&out).unwrap().same_tables(&new));
        assert_eq!(patch(src, &parse(src).unwrap()), src, "nothing to do");

        let mut renamed = parse(src).unwrap();
        renamed.rename_table("b", "bee");
        let out = patch_renamed(src, &renamed, &[("b".into(), "bee".into())]);
        assert_eq!(out, src.replace("table b {\n  y int\n}", "table bee {\n  y  int\n}"));

        // Hints survive a new schema that has none.
        let hinted = "table a was old { # keep me\n  x int pk was y\n}\n";
        let mut new = parse(hinted).unwrap();
        new.tables[0].was.clear();
        new.tables[0].columns[0].was.clear();
        assert_eq!(patch(hinted, &new), hinted);
        new.tables[0].columns[0].unique = true;
        assert_eq!(patch(hinted, &new), "table a was old {\n  x  int  pk unique was y\n}\n");
    }
}

