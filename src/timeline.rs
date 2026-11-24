//! A project's schema history: one [`Step`] per migration file, each holding
//! the schema as it stands after that file, and what changed in it
//! ([`changes`]). Filled by [`crate::laravel::read_dir`].

use crate::schema::{Column, Schema, Table};
use std::path::PathBuf;

/// Names a migration changed, oldest first, so [`changes`] can tell a rename
/// from a drop plus a create.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Renames {
    /// (old, new) table names.
    pub tables: Vec<(String, String)>,
    /// (table, old, new); the table as it is named after the step.
    pub columns: Vec<(String, String, String)>,
}

impl Renames {
    pub fn table(&mut self, old: &str, new: &str) {
        for c in self.columns.iter_mut().filter(|c| c.0 == old) {
            c.0 = new.to_string();
        }
        self.tables.push((old.to_string(), new.to_string()));
    }

    pub fn column(&mut self, table: &str, old: &str, new: &str) {
        self.columns.push((table.to_string(), old.to_string(), new.to_string()));
    }

    /// The name a table had before the step.
    pub fn old_table(&self, name: &str) -> String {
        self.tables.iter().rev().fold(name.to_string(), |n, (old, new)| if *new == n { old.clone() } else { n })
    }

    /// The name a column of `table` (its name after the step) had before it.
    pub fn old_column(&self, table: &str, name: &str) -> String {
        self.columns.iter().rev().filter(|c| c.0 == table).fold(name.to_string(), |n, (_, old, new)| if *new == n { old.clone() } else { n })
    }
}

/// One migration file and the schema after it.
#[derive(Debug, Clone)]
pub struct Step {
    pub file: PathBuf,
    /// The file name without extension: `2014_10_12_000000_create_users_table`.
    pub id: String,
    /// `2014-10-12`, from a Laravel-style timestamp prefix.
    pub date: Option<String>,
    /// `create users table`.
    pub title: String,
    pub schema: Schema,
    pub renames: Renames,
    /// What the step does that the schema can't show (raw SQL, unknown calls).
    pub warnings: Vec<String>,
}

/// How a table or column fared in one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Unchanged,
    Added,
    /// Renamed, and nothing else.
    Renamed,
    Altered,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableMarks {
    pub mark: Mark,
    /// The name before the step, when it changed.
    pub was: Option<String>,
    /// Marks of the columns that changed, by name (after the step; dropped ones by their old name).
    pub columns: Vec<(String, Mark)>,
    /// Readable changes: `+ column avatar varchar(255)`.
    pub details: Vec<String>,
}

impl TableMarks {
    pub fn column(&self, name: &str) -> Mark {
        self.columns.iter().find(|c| c.0 == name).map_or(Mark::Unchanged, |c| c.1)
    }
}

/// What one step did, and a schema to draw it with.
#[derive(Debug, Clone, Default)]
pub struct Changes {
    /// The schema after the step plus what it dropped: dropped columns at the
    /// end of their tables, dropped tables at the end.
    pub shown: Schema,
    /// Marks by table name (as in `shown`); tables left out are unchanged.
    pub tables: Vec<(String, TableMarks)>,
}

impl Changes {
    pub fn table(&self, name: &str) -> Option<&TableMarks> {
        self.tables.iter().find(|t| t.0 == name).map(|t| &t.1)
    }

    /// Counts of added, altered (or renamed) and dropped tables.
    pub fn counts(&self) -> (usize, usize, usize) {
        let n = |m: &[Mark]| self.tables.iter().filter(|t| m.contains(&t.1.mark)).count();
        (n(&[Mark::Added]), n(&[Mark::Altered, Mark::Renamed]), n(&[Mark::Dropped]))
    }
}

/// What turned `before` into `after`, with `renames` telling renames apart.
pub fn changes(before: &Schema, after: &Schema, renames: &Renames) -> Changes {
    let mut shown = after.clone();
    let mut tables = Vec::new();
    let mut matched: Vec<String> = Vec::new();
    for t in &after.tables {
        let old_name = renames.old_table(&t.name);
        let Some(old) = before.table(&old_name) else {
            let details = t.columns.iter().map(|c| format!("+ {}", column_text(c))).collect();
            tables.push((t.name.clone(), TableMarks { mark: Mark::Added, was: None, columns: vec![], details }));
            continue;
        };
        matched.push(old_name.clone());
        let renamed = old_name != t.name;
        let mut marks = TableMarks { mark: Mark::Unchanged, was: renamed.then(|| old_name.clone()), columns: vec![], details: vec![] };
        if renamed {
            marks.details.push(format!("renamed from {old_name}"));
        }
        let mut kept: Vec<&str> = Vec::new();
        for c in &t.columns {
            let old_col = renames.old_column(&t.name, &c.name);
            match old.column(&old_col) {
                None => {
                    marks.columns.push((c.name.clone(), Mark::Added));
                    marks.details.push(format!("+ column {}", column_text(c)));
                }
                Some(o) => {
                    kept.push(&o.name);
                    let what = column_diff(o, c);
                    if !what.is_empty() {
                        marks.columns.push((c.name.clone(), Mark::Altered));
                        let rename = if o.name != c.name { format!(" (was {})", o.name) } else { String::new() };
                        marks.details.push(format!("~ column {}{rename}: {}", c.name, what.join(", ")));
                    } else if o.name != c.name {
                        marks.columns.push((c.name.clone(), Mark::Renamed));
                        marks.details.push(format!("~ column {} renamed to {}", o.name, c.name));
                    }
                }
            }
        }
        let shown_table = shown.tables.iter_mut().find(|s| s.name == t.name).unwrap();
        for o in old.columns.iter().filter(|o| !kept.contains(&o.name.as_str())) {
            marks.columns.push((o.name.clone(), Mark::Dropped));
            marks.details.push(format!("− column {}", o.name));
            if shown_table.column(&o.name).is_none() {
                let mut ghost = o.clone();
                ghost.references = None;
                shown_table.columns.push(ghost);
            }
        }
        let table_level = table_level_diff(old, t, renames);
        marks.mark = if !table_level.is_empty() || !marks.columns.is_empty() {
            Mark::Altered
        } else if renamed {
            Mark::Renamed
        } else {
            Mark::Unchanged
        };
        marks.details.extend(table_level);
        if marks.mark != Mark::Unchanged {
            tables.push((t.name.clone(), marks));
        }
    }
    for old in before.tables.iter().filter(|o| !matched.contains(&o.name)) {
        let mut ghost = old.clone();
        // Keys to tables that are gone too would dangle; the box is what matters.
        for c in &mut ghost.columns {
            if c.references.as_ref().is_some_and(|fk| after.table(&fk.table).is_none() && before.table(&fk.table).is_none()) {
                c.references = None;
            }
        }
        if shown.table(&ghost.name).is_none() {
            shown.tables.push(ghost);
        }
        tables.push((old.name.clone(), TableMarks { mark: Mark::Dropped, was: None, columns: vec![], details: vec!["dropped".into()] }));
    }
    Changes { shown, tables }
}

/// `email varchar(255) not null -> users.id`.
pub fn column_text(c: &Column) -> String {
    let mut s = format!("{} {}", c.name, crate::format::type_text(&c.name, &c.data_type));
    if c.primary_key {
        s.push_str(" pk");
    }
    if !c.nullable && !c.primary_key {
        s.push_str(" not null");
    }
    if let Some(d) = &c.default {
        s.push_str(&format!(" default {d}"));
    }
    if let Some(fk) = &c.references {
        s.push_str(&format!(" -> {}.{}", fk.table, fk.column));
    }
    s
}

fn column_diff(o: &Column, n: &Column) -> Vec<String> {
    let mut out = Vec::new();
    if o.data_type != n.data_type {
        out.push(format!("type {} → {}", o.data_type, n.data_type));
    }
    if o.nullable != n.nullable {
        out.push(if n.nullable { "now nullable".into() } else { "now not null".into() });
    }
    if o.primary_key != n.primary_key {
        out.push(if n.primary_key { "now primary key".into() } else { "no longer primary key".into() });
    }
    if o.default != n.default {
        out.push(match &n.default {
            Some(d) => format!("default {d}"),
            None => "default removed".into(),
        });
    }
    if o.references != n.references {
        out.push(match &n.references {
            Some(fk) => format!("references {}.{}", fk.table, fk.column),
            None => "reference removed".into(),
        });
    }
    if o.unique != n.unique {
        out.push(if n.unique { "now unique".into() } else { "no longer unique".into() });
    }
    if o.comment != n.comment {
        out.push("comment changed".into());
    }
    if o.auto_increment != n.auto_increment {
        out.push(if n.auto_increment { "now auto-increment".into() } else { "no longer auto-increment".into() });
    }
    out
}

/// Keys, indexes and comment; columns of the old side read under their new names.
fn table_level_diff(old: &Table, new: &Table, renames: &Renames) -> Vec<String> {
    let mut s = Schema { tables: vec![old.clone()] };
    for (_, from, to) in renames.columns.iter().filter(|c| c.0 == new.name) {
        s.rename_column(&old.name, from, to);
    }
    let old = s.tables.remove(0);
    let mut out = Vec::new();
    if old.composite_pk != new.composite_pk {
        out.push(format!("primary key ({})", new.composite_pk.join(", ")));
    }
    for i in &new.indexes {
        if !old.indexes.contains(i) {
            out.push(format!("+ {}index {} ({})", if i.unique { "unique " } else { "" }, i.name, i.columns.join(", ")));
        }
    }
    for i in &old.indexes {
        if !new.indexes.iter().any(|n| n.name == i.name) {
            out.push(format!("− index {}", i.name));
        }
    }
    for fk in &new.foreign_keys {
        if !old.foreign_keys.contains(fk) {
            out.push(format!("+ foreign key ({}) -> {} ({})", fk.columns.join(", "), fk.table, fk.ref_columns.join(", ")));
        }
    }
    for fk in &old.foreign_keys {
        if !new.foreign_keys.contains(fk) {
            out.push(format!("− foreign key ({})", fk.columns.join(", ")));
        }
    }
    if old.comment != new.comment {
        out.push("comment changed".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    #[test]
    fn marks_added_altered_renamed_and_dropped() {
        let before = parse("table users {\n  id int pk\n  name text\n  old text\n}\ntable tags {\n  id int pk\n}\ntable logs {\n  id int pk\n}\n").unwrap();
        let after = parse("table people {\n  id int pk\n  full_name text\n  email text\n}\ntable tags {\n  id int pk\n}\ntable posts {\n  id int pk\n  author int -> people.id\n}\n").unwrap();
        let mut r = Renames::default();
        r.column("users", "name", "full_name");
        r.table("users", "people");
        let c = changes(&before, &after, &r);
        let people = c.table("people").unwrap();
        assert_eq!(people.mark, Mark::Altered);
        assert_eq!(people.was.as_deref(), Some("users"));
        assert_eq!(people.column("full_name"), Mark::Renamed);
        assert_eq!(people.column("email"), Mark::Added);
        assert_eq!(people.column("old"), Mark::Dropped);
        assert!(c.shown.table("people").unwrap().column("old").is_some());
        assert!(c.table("tags").is_none());
        assert_eq!(c.table("posts").unwrap().mark, Mark::Added);
        assert_eq!(c.table("logs").unwrap().mark, Mark::Dropped);
        assert!(c.shown.table("logs").is_some());
        assert_eq!(c.counts(), (1, 1, 1));
    }

    #[test]
    fn a_plain_rename_is_renamed() {
        let before = parse("table a {\n  id int pk\n}\n").unwrap();
        let after = parse("table b {\n  id int pk\n}\n").unwrap();
        let mut r = Renames::default();
        r.table("a", "b");
        assert_eq!(changes(&before, &after, &r).table("b").unwrap().mark, Mark::Renamed);
    }
}
