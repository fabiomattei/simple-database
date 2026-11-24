//! Raw SQL in migrations (`DB::statement`, `DB::unprepared`) applied to the
//! schema where it is plain DDL: `ALTER TABLE` (MySQL `ADD` / `MODIFY` /
//! `CHANGE` / `DROP` / `RENAME`, Postgres `ALTER COLUMN`), `CREATE TABLE`,
//! `RENAME TABLE`, `DROP TABLE`, `CREATE INDEX`, `DROP INDEX`. Statements that
//! can't change the schema (`SET`, `UPDATE`, …) are skipped quietly; anything
//! else, or a part of an `ALTER TABLE` it can't read, comes back as a warning.

use super::{index_name, short};
use crate::schema::{Column, DataType, DefaultValue, ForeignKey, Index, RefAction, Schema, TableForeignKey};
use crate::timeline::Renames;

/// Applies `sql` (one or more statements) to `schema`; returns warnings.
pub(super) fn apply(schema: &mut Schema, sql: &str, renames: &mut Renames) -> Vec<String> {
    let mut warnings = Vec::new();
    for stmt in crate::db::split_statements(sql) {
        let toks = tokenize(&stmt);
        let mut s = Stmt { t: &toks, i: 0 };
        let outcome = s.statement(schema, renames);
        match outcome {
            Outcome::Done => {}
            Outcome::Skipped(parts) => warnings.push(format!("raw SQL partly applied: {} (not understood: {})", short(&stmt), parts.join("; "))),
            Outcome::Unknown => warnings.push(format!("raw SQL not applied: {}", short(&stmt))),
        }
    }
    warnings
}

enum Outcome {
    Done,
    /// Applied, except these parts.
    Skipped(Vec<String>),
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
enum T {
    /// A word or name; `quoted` for `` `x` `` and `"x"`.
    Word { text: String, quoted: bool },
    Str(String),
    Num(String),
    P(char),
}

fn tokenize(sql: &str) -> Vec<T> {
    let c: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '-' && c.get(i + 1) == Some(&'-') || ch == '#' {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if ch == '/' && c.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < c.len() && !(c[i] == '*' && c[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if ch == '\'' {
            let mut s = String::new();
            i += 1;
            while i < c.len() {
                if c[i] == '\\' && i + 1 < c.len() {
                    s.push(c[i + 1]);
                    i += 2;
                } else if c[i] == '\'' && c.get(i + 1) == Some(&'\'') {
                    s.push('\'');
                    i += 2;
                } else if c[i] == '\'' {
                    break;
                } else {
                    s.push(c[i]);
                    i += 1;
                }
            }
            i += 1;
            out.push(T::Str(s));
        } else if ch == '`' || ch == '"' {
            let mut s = String::new();
            i += 1;
            while i < c.len() && c[i] != ch {
                s.push(c[i]);
                i += 1;
            }
            i += 1;
            out.push(T::Word { text: s, quoted: true });
        } else if ch.is_ascii_digit() {
            let start = i;
            while i < c.len() && (c[i].is_ascii_digit() || c[i] == '.') {
                i += 1;
            }
            out.push(T::Num(c[start..i].iter().collect()));
        } else if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$') {
                i += 1;
            }
            out.push(T::Word { text: c[start..i].iter().collect(), quoted: false });
        } else {
            out.push(T::P(ch));
            i += 1;
        }
    }
    out
}

/// Words after which a column definition's type ends.
const OPTION_WORDS: [&str; 25] = [
    "not", "null", "default", "primary", "unique", "auto_increment", "autoincrement", "comment", "collate", "character", "charset", "references", "check", "after",
    "first", "on", "generated", "as", "constraint", "key", "identity", "invisible", "visible", "storage", "using",
];

/// Statements that read or write rows, or set the session: never a schema change.
const NOT_SCHEMA: [&str; 20] = [
    "set", "update", "insert", "delete", "select", "replace", "truncate", "lock", "unlock", "analyze", "optimize", "flush", "grant", "revoke", "commit", "begin",
    "start", "rollback", "savepoint", "pragma",
];

struct Stmt<'a> {
    t: &'a [T],
    i: usize,
}

impl Stmt<'_> {
    fn peek(&self) -> Option<&T> {
        self.t.get(self.i)
    }

    /// Whether the next token is the (unquoted) keyword `k`.
    fn at(&self, k: &str) -> bool {
        matches!(self.peek(), Some(T::Word { text, quoted: false }) if text.eq_ignore_ascii_case(k))
    }

    fn eat(&mut self, k: &str) -> bool {
        let yes = self.at(k);
        if yes {
            self.i += 1;
        }
        yes
    }

    fn eat_all(&mut self, words: &[&str]) -> bool {
        let start = self.i;
        if words.iter().all(|w| self.eat(w)) {
            return true;
        }
        self.i = start;
        false
    }

    fn punct(&mut self, ch: char) -> bool {
        let yes = self.peek() == Some(&T::P(ch));
        if yes {
            self.i += 1;
        }
        yes
    }

    /// A name, possibly `schema.table` (the schema part is dropped).
    fn name(&mut self) -> Option<String> {
        let mut name = match self.peek()? {
            T::Word { text, .. } => text.clone(),
            _ => return None,
        };
        self.i += 1;
        while self.peek() == Some(&T::P('.')) {
            self.i += 1;
            if let Some(T::Word { text, .. }) = self.peek() {
                name = text.clone();
                self.i += 1;
            }
        }
        Some(name)
    }

    /// `(a, b)` → names; expressions are kept in parentheses.
    fn name_list(&mut self) -> Option<Vec<String>> {
        if !self.punct('(') {
            return None;
        }
        let mut out = Vec::new();
        loop {
            let start = self.i;
            let mut depth = 0;
            while let Some(t) = self.peek() {
                match t {
                    T::P('(') => depth += 1,
                    T::P(')') if depth == 0 => break,
                    T::P(')') => depth -= 1,
                    T::P(',') if depth == 0 => break,
                    _ => {}
                }
                self.i += 1;
            }
            let part = &self.t[start..self.i];
            match part {
                [T::Word { text, .. }] | [T::Word { text, .. }, T::P('('), T::Num(_), T::P(')')] => out.push(text.clone()),
                [T::Word { text, .. }, T::Word { text: dir, quoted: false }] if dir.eq_ignore_ascii_case("asc") || dir.eq_ignore_ascii_case("desc") => out.push(text.clone()),
                _ => out.push(Index::expression_part(&text(part))),
            }
            if self.punct(',') {
                continue;
            }
            self.punct(')');
            return Some(out);
        }
    }

    /// Moves past the current clause: to the next `,` outside parentheses, or the end.
    fn skip_clause(&mut self) -> String {
        let start = self.i;
        let mut depth = 0;
        while let Some(t) = self.peek() {
            match t {
                T::P('(') => depth += 1,
                T::P(')') => depth -= 1,
                T::P(',') if depth == 0 => break,
                _ => {}
            }
            self.i += 1;
        }
        text(&self.t[start..self.i])
    }

    fn statement(&mut self, schema: &mut Schema, renames: &mut Renames) -> Outcome {
        if NOT_SCHEMA.iter().any(|k| self.at(k)) {
            return Outcome::Done;
        }
        if self.eat_all(&["alter", "table"]) {
            self.eat("only");
            self.eat_all(&["if", "exists"]);
            let Some(table) = self.name() else { return Outcome::Unknown };
            if schema.table(&table).is_none() {
                return Outcome::Unknown;
            }
            return self.alter(schema, table, renames);
        }
        if self.eat_all(&["rename", "table"]) {
            loop {
                let (Some(from), true, Some(to)) = (self.name(), self.eat("to"), self.name()) else { return Outcome::Unknown };
                if schema.table(&from).is_none() {
                    return Outcome::Unknown;
                }
                schema.rename_table(&from, &to);
                renames.table(&from, &to);
                if !self.punct(',') {
                    return Outcome::Done;
                }
            }
        }
        if self.eat("drop") {
            if self.eat("table") {
                self.eat_all(&["if", "exists"]);
                loop {
                    let Some(t) = self.name() else { return Outcome::Unknown };
                    schema.drop_table(&t);
                    if !self.punct(',') {
                        return Outcome::Done;
                    }
                }
            }
            if self.eat("index") {
                self.eat("concurrently");
                self.eat_all(&["if", "exists"]);
                let Some(name) = self.name() else { return Outcome::Unknown };
                let on = if self.eat("on") { self.name() } else { None };
                let mut found = false;
                for t in schema.tables.iter_mut().filter(|t| on.as_ref().is_none_or(|o| *o == t.name)) {
                    let before = t.indexes.len();
                    t.indexes.retain(|i| i.name != name);
                    found |= t.indexes.len() != before;
                }
                return if found { Outcome::Done } else { Outcome::Unknown };
            }
            return Outcome::Unknown;
        }
        if self.eat("create") {
            let unique = self.eat("unique");
            if self.eat("index") {
                self.eat("concurrently");
                self.eat_all(&["if", "not", "exists"]);
                let name = if self.at("on") { None } else { self.name() };
                let (true, Some(table)) = (self.eat("on"), self.name()) else { return Outcome::Unknown };
                let method = if self.eat("using") { self.name().map(|m| m.to_ascii_lowercase()) } else { None };
                let Some(cols) = self.name_list() else { return Outcome::Unknown };
                let Some(t) = schema.tables.iter_mut().find(|t| t.name == table) else { return Outcome::Unknown };
                let mut index = Index::new(name.unwrap_or_else(|| Index::default_name(&table, &cols, unique)), cols, unique);
                index.method = method.filter(|m| m != "btree");
                if self.eat("where") {
                    index.predicate = Some(self.skip_clause());
                }
                t.indexes.retain(|i| i.name != index.name);
                t.indexes.push(index);
                return Outcome::Done;
            }
            if !unique && self.eat("table") {
                self.eat_all(&["if", "not", "exists"]);
                let Some(name) = self.name() else { return Outcome::Unknown };
                if !self.punct('(') {
                    return Outcome::Unknown;
                }
                schema.drop_table(&name);
                schema.tables.push(crate::schema::Table::new(name.clone()));
                let mut skipped = Vec::new();
                loop {
                    if let Err(part) = self.item(schema, &name) {
                        skipped.push(part);
                    }
                    if !self.punct(',') {
                        break;
                    }
                }
                return if skipped.is_empty() { Outcome::Done } else { Outcome::Skipped(skipped) };
            }
        }
        Outcome::Unknown
    }

    /// The actions of an `ALTER TABLE`, comma separated.
    fn alter(&mut self, schema: &mut Schema, mut table: String, renames: &mut Renames) -> Outcome {
        let mut skipped = Vec::new();
        let mut applied = 0;
        loop {
            let start = self.i;
            match self.action(schema, &mut table, renames) {
                Ok(()) => applied += 1,
                Err(()) => {
                    self.i = start;
                    skipped.push(self.skip_clause());
                }
            }
            // Whatever follows an action it read (`ALGORITHM=…`, `LOCK=…`) is left alone.
            if !self.punct(',') {
                let rest = self.skip_clause();
                if !rest.is_empty() {
                    skipped.push(rest);
                }
                if !self.punct(',') {
                    break;
                }
            }
        }
        match (applied, skipped.is_empty()) {
            (_, true) => Outcome::Done,
            (0, false) => Outcome::Unknown,
            _ => Outcome::Skipped(skipped),
        }
    }

    fn action(&mut self, schema: &mut Schema, table: &mut String, renames: &mut Renames) -> Result<(), ()> {
        let t = table.clone();
        if self.eat("add") {
            if self.at("column") || !self.at_constraint_word() {
                self.eat("column");
                self.eat_all(&["if", "not", "exists"]);
                let (col, after) = self.column_def()?;
                let tb = schema.tables.iter_mut().find(|x| x.name == t).ok_or(())?;
                tb.columns.retain(|c| c.name != col.name);
                let at = match after {
                    Some(Some(a)) => tb.columns.iter().position(|c| c.name == a).map_or(tb.columns.len(), |p| p + 1),
                    Some(None) => 0,
                    None => tb.columns.len(),
                };
                tb.columns.insert(at, col);
                return Ok(());
            }
            return self.item(schema, &t).map_err(|_| ());
        }
        if self.eat("modify") {
            self.eat("column");
            let (col, after) = self.column_def()?;
            return redefine(schema, &t, &col.name.clone(), col, after);
        }
        if self.eat("change") {
            self.eat("column");
            let old = self.name().ok_or(())?;
            let (col, after) = self.column_def()?;
            let new = col.name.clone();
            if schema.table(&t).and_then(|x| x.column(&old)).is_none() {
                return Err(());
            }
            if old != new {
                schema.rename_column(&t, &old, &new);
                renames.column(&t, &old, &new);
            }
            return redefine(schema, &t, &new, col, after);
        }
        if self.eat("alter") {
            // Postgres: ALTER [COLUMN] x TYPE … / SET NOT NULL / DROP NOT NULL / SET DEFAULT … / DROP DEFAULT
            self.eat("column");
            let name = self.name().ok_or(())?;
            let tb = schema.tables.iter_mut().find(|x| x.name == t).ok_or(())?;
            let c = tb.columns.iter_mut().find(|c| c.name == name).ok_or(())?;
            if self.eat("type") || self.eat_all(&["set", "data", "type"]) {
                c.data_type = data_type(&self.type_text());
                if self.eat("using") {
                    self.skip_clause();
                }
            } else if self.eat_all(&["set", "not", "null"]) {
                c.nullable = false;
            } else if self.eat_all(&["drop", "not", "null"]) {
                c.nullable = true;
            } else if self.eat_all(&["set", "default"]) {
                c.default = default_value(self);
            } else if self.eat_all(&["drop", "default"]) {
                c.default = None;
            } else {
                return Err(());
            }
            return Ok(());
        }
        if self.eat("drop") {
            if self.eat_all(&["primary", "key"]) {
                let tb = schema.tables.iter_mut().find(|x| x.name == t).ok_or(())?;
                tb.composite_pk.clear();
                for c in &mut tb.columns {
                    c.primary_key = false;
                    c.auto_increment = false;
                }
                return Ok(());
            }
            if self.eat_all(&["foreign", "key"]) || self.eat("constraint") {
                self.eat_all(&["if", "exists"]);
                let name = self.name().ok_or(())?;
                let tb = schema.tables.iter_mut().find(|x| x.name == t).ok_or(())?;
                if let Some(cols) = tb.all_foreign_keys().into_iter().map(|fk| fk.columns).find(|cols| {
                    name == index_name(&t, cols, "foreign") || name == format!("fk_{}_{}", crate::schema::unqualified(&t), cols.join("_"))
                }) {
                    if let [one] = cols.as_slice()
                        && let Some(c) = tb.columns.iter_mut().find(|c| c.name == *one)
                    {
                        c.references = None;
                    }
                    tb.foreign_keys.retain(|fk| fk.columns != cols);
                    return Ok(());
                }
                // A unique or check constraint by that name.
                let before = tb.indexes.len();
                tb.indexes.retain(|i| i.name != name);
                return if tb.indexes.len() != before { Ok(()) } else { Err(()) };
            }
            if self.eat("index") || self.eat("key") {
                let name = self.name().ok_or(())?;
                let tb = schema.tables.iter_mut().find(|x| x.name == t).ok_or(())?;
                let before = tb.indexes.len();
                tb.indexes.retain(|i| i.name != name);
                return if tb.indexes.len() != before { Ok(()) } else { Err(()) };
            }
            self.eat("column");
            self.eat_all(&["if", "exists"]);
            let name = self.name().ok_or(())?;
            schema.table(&t).and_then(|x| x.column(&name)).ok_or(())?;
            schema.drop_column(&t, &name);
            self.eat("cascade");
            return Ok(());
        }
        if self.eat("rename") {
            if self.eat("column") {
                let (Some(old), true, Some(new)) = (self.name(), self.eat("to"), self.name()) else { return Err(()) };
                schema.table(&t).and_then(|x| x.column(&old)).ok_or(())?;
                schema.rename_column(&t, &old, &new);
                renames.column(&t, &old, &new);
                return Ok(());
            }
            if self.eat("index") || self.eat("key") {
                let (Some(old), true, Some(new)) = (self.name(), self.eat("to"), self.name()) else { return Err(()) };
                let tb = schema.tables.iter_mut().find(|x| x.name == t).ok_or(())?;
                tb.indexes.iter_mut().find(|i| i.name == old).ok_or(())?.name = new;
                return Ok(());
            }
            if !self.eat("to") {
                self.eat("as");
            }
            let new = self.name().ok_or(())?;
            schema.rename_table(&t, &new);
            renames.table(&t, &new);
            *table = new;
            return Ok(());
        }
        Err(())
    }

    fn at_constraint_word(&self) -> bool {
        ["constraint", "primary", "unique", "index", "key", "foreign", "fulltext", "spatial", "check"].iter().any(|w| self.at(w))
    }

    /// One item of `CREATE TABLE (…)` or `ALTER TABLE … ADD`: a column, a key, an index.
    /// `Err` holds the text it couldn't read.
    fn item(&mut self, schema: &mut Schema, t: &str) -> Result<(), String> {
        let start = self.i;
        let fail = |s: &mut Self| {
            s.i = start;
            Err(s.skip_clause())
        };
        let constraint = if self.eat("constraint") { self.name() } else { None };
        if self.eat_all(&["primary", "key"]) {
            let Some(cols) = self.name_list() else { return fail(self) };
            let Some(tb) = schema.tables.iter_mut().find(|x| x.name == t) else { return fail(self) };
            for c in &mut tb.columns {
                c.primary_key = false;
            }
            tb.composite_pk.clear();
            match cols.as_slice() {
                [one] => {
                    if let Some(c) = tb.columns.iter_mut().find(|c| c.name == *one) {
                        c.primary_key = true;
                        c.nullable = false;
                    }
                }
                _ => tb.composite_pk = cols,
            }
            self.skip_clause();
            return Ok(());
        }
        if self.eat_all(&["foreign", "key"]) {
            if !matches!(self.peek(), Some(T::P('('))) {
                self.name();
            }
            let (Some(cols), true, Some(target), Some(refs)) = (self.name_list(), self.eat("references"), self.name(), self.name_list()) else { return fail(self) };
            let (on_delete, on_update) = self.ref_actions();
            let Some(tb) = schema.tables.iter_mut().find(|x| x.name == t) else { return fail(self) };
            if let ([one], [to]) = (cols.as_slice(), refs.as_slice())
                && let Some(c) = tb.columns.iter_mut().find(|c| c.name == *one)
            {
                c.references = Some(ForeignKey { table: target, column: to.clone(), on_delete, on_update });
            } else {
                tb.foreign_keys.retain(|fk| fk.columns != cols);
                tb.foreign_keys.push(TableForeignKey { columns: cols, table: target, ref_columns: refs, on_delete, on_update });
            }
            return Ok(());
        }
        if self.at("unique") || self.at("index") || self.at("key") || self.at("fulltext") || self.at("spatial") {
            let unique = self.eat("unique");
            self.eat("fulltext");
            self.eat("spatial");
            if !self.eat("index") {
                self.eat("key");
            }
            let name = if matches!(self.peek(), Some(T::P('('))) { None } else { self.name() };
            if self.eat("using") {
                self.name();
            }
            let Some(cols) = self.name_list() else { return fail(self) };
            let Some(tb) = schema.tables.iter_mut().find(|x| x.name == t) else { return fail(self) };
            // MySQL names an unnamed index after its first column.
            let name = name.or(constraint).unwrap_or_else(|| cols[0].clone());
            tb.indexes.retain(|i| i.name != name);
            tb.indexes.push(Index::new(name, cols, unique));
            self.skip_clause();
            return Ok(());
        }
        if constraint.is_some() || self.at("check") {
            return fail(self);
        }
        let Ok((col, _)) = self.column_def() else { return fail(self) };
        let Some(tb) = schema.tables.iter_mut().find(|x| x.name == t) else { return fail(self) };
        tb.columns.retain(|c| c.name != col.name);
        tb.columns.push(col);
        Ok(())
    }

    /// `ON DELETE …` / `ON UPDATE …` after a reference.
    fn ref_actions(&mut self) -> (RefAction, RefAction) {
        let (mut delete, mut update) = (RefAction::NoAction, RefAction::NoAction);
        while self.eat("on") {
            let is_delete = self.eat("delete");
            if !is_delete {
                self.eat("update");
            }
            let action = if self.eat_all(&["set", "null"]) {
                RefAction::SetNull
            } else if self.eat_all(&["set", "default"]) {
                RefAction::SetDefault
            } else if self.eat_all(&["no", "action"]) {
                RefAction::NoAction
            } else if self.eat("cascade") {
                RefAction::Cascade
            } else if self.eat("restrict") {
                RefAction::Restrict
            } else {
                RefAction::NoAction
            };
            if is_delete { delete = action } else { update = action }
        }
        (delete, update)
    }

    /// The type of a column definition as text: `varchar(36)`, `enum('a','b')`, `int unsigned`.
    fn type_text(&mut self) -> String {
        let mut out = String::new();
        while let Some(tok) = self.peek() {
            match tok {
                T::Word { text, quoted: false } if out.is_empty() || !OPTION_WORDS.contains(&text.to_ascii_lowercase().as_str()) => {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(&text.to_ascii_lowercase());
                    self.i += 1;
                }
                T::P('(') => {
                    let start = self.i;
                    let mut depth = 0;
                    while let Some(t) = self.peek() {
                        match t {
                            T::P('(') => depth += 1,
                            T::P(')') => depth -= 1,
                            _ => {}
                        }
                        self.i += 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    out.push_str(&text(&self.t[start..self.i]).replace(' ', ""));
                }
                T::P('[') => {
                    out.push_str("[]");
                    self.i += 2;
                }
                _ => break,
            }
        }
        out
    }

    /// `name type options…`; also where `AFTER x` / `FIRST` puts it
    /// (`Some(None)` for first).
    #[allow(clippy::type_complexity)]
    fn column_def(&mut self) -> Result<(Column, Option<Option<String>>), ()> {
        let name = match self.peek() {
            Some(T::Word { text, .. }) => text.clone(),
            _ => return Err(()),
        };
        self.i += 1;
        let ty = self.type_text();
        if ty.is_empty() {
            return Err(());
        }
        let mut c = Column::new(name, data_type(&ty));
        let mut after = None;
        loop {
            if self.eat_all(&["not", "null"]) {
                c.nullable = false;
            } else if self.eat("null") {
                c.nullable = true;
            } else if self.eat("default") {
                c.default = default_value(self);
            } else if self.eat_all(&["primary", "key"]) {
                c.primary_key = true;
                c.nullable = false;
            } else if self.eat("unique") {
                self.eat("key");
                c.unique = true;
            } else if self.eat("auto_increment") || self.eat("autoincrement") {
                c.auto_increment = true;
            } else if self.eat_all(&["generated", "by", "default", "as", "identity"]) || self.eat_all(&["generated", "always", "as", "identity"]) {
                c.auto_increment = true;
                if matches!(self.peek(), Some(T::P('('))) {
                    self.name_list();
                }
            } else if self.eat("comment") {
                if let Some(T::Str(s)) = self.peek() {
                    c.comment = Some(s.clone()).filter(|s| !s.is_empty());
                    self.i += 1;
                }
            } else if self.eat("collate") || self.eat("charset") || self.eat_all(&["character", "set"]) {
                self.name();
            } else if self.eat_all(&["on", "update"]) {
                self.name();
                if matches!(self.peek(), Some(T::P('('))) {
                    self.name_list();
                }
            } else if self.eat("references") {
                let target = self.name().ok_or(())?;
                let col = self.name_list().and_then(|l| l.into_iter().next()).unwrap_or_else(|| "id".into());
                let (on_delete, on_update) = self.ref_actions();
                c.references = Some(ForeignKey { table: target, column: col, on_delete, on_update });
            } else if self.eat("after") {
                after = Some(self.name());
            } else if self.eat("first") {
                after = Some(None);
            } else if self.eat("visible") || self.eat("invisible") {
            } else if self.at("check") || self.at("generated") || self.at("as") || self.at("constraint") {
                self.skip_clause();
            } else {
                break;
            }
        }
        if c.primary_key || matches!(self.peek(), None | Some(T::P(',' | ')'))) {
            return Ok((c, after));
        }
        Err(())
    }
}

/// `MODIFY` / `CHANGE`: a new definition for an existing column. Keys and
/// indexes stay, as they are separate in MySQL.
fn redefine(schema: &mut Schema, t: &str, name: &str, new: Column, after: Option<Option<String>>) -> Result<(), ()> {
    let tb = schema.tables.iter_mut().find(|x| x.name == t).ok_or(())?;
    let pos = tb.columns.iter().position(|c| c.name == name).ok_or(())?;
    let old = &mut tb.columns[pos];
    old.data_type = new.data_type;
    old.nullable = new.nullable && !old.primary_key;
    old.default = new.default;
    old.comment = new.comment;
    old.auto_increment |= new.auto_increment;
    old.unique |= new.unique;
    if new.references.is_some() {
        old.references = new.references;
    }
    if let Some(after) = after {
        let col = tb.columns.remove(pos);
        let at = match after {
            Some(a) => tb.columns.iter().position(|c| c.name == a).map_or(tb.columns.len(), |p| p + 1),
            None => 0,
        };
        tb.columns.insert(at, col);
    }
    Ok(())
}

/// A SQL type as sdb's, with MySQL's spellings: `tinyint(1)` is a boolean,
/// `longtext` text, `unsigned` dropped.
fn data_type(ty: &str) -> DataType {
    let t = ty.replace(" unsigned", "").replace(" zerofill", "");
    match t.as_str() {
        "tinyint(1)" | "bit(1)" => DataType::Bool,
        "tinytext" | "mediumtext" | "longtext" => DataType::Text,
        "mediumblob" | "longblob" | "tinyblob" | "varbinary" => DataType::Blob,
        _ if t.starts_with("varbinary(") || t.starts_with("binary(") => DataType::Blob,
        _ => DataType::parse(&t),
    }
}

/// The value after `DEFAULT`.
fn default_value(s: &mut Stmt) -> Option<DefaultValue> {
    let v = match s.peek()? {
        T::Str(v) => DefaultValue::String(v.clone()),
        T::Num(n) => DefaultValue::Number(n.clone()),
        T::P('-') => {
            s.i += 1;
            match s.peek()? {
                T::Num(n) => DefaultValue::Number(format!("-{n}")),
                _ => return None,
            }
        }
        T::P('(') => {
            let start = s.i;
            s.name_list();
            let inner = text(&s.t[start + 1..s.i - 1]);
            return Some(DefaultValue::Expr(inner));
        }
        T::Word { text, quoted: false } => match text.to_ascii_lowercase().as_str() {
            "null" => {
                s.i += 1;
                return None;
            }
            "true" | "false" => DefaultValue::Bool(text.eq_ignore_ascii_case("true")),
            "current_timestamp" | "now" | "localtimestamp" => {
                s.i += 1;
                if matches!(s.peek(), Some(T::P('('))) {
                    s.name_list();
                }
                return Some(DefaultValue::Now);
            }
            w => DefaultValue::Expr(w.to_string()),
        },
        _ => return None,
    };
    s.i += 1;
    // Postgres casts: DEFAULT 'x'::text
    while s.peek() == Some(&T::P(':')) {
        s.i += 2;
        s.type_text();
    }
    Some(v)
}

/// Tokens back as SQL text.
fn text(t: &[T]) -> String {
    let mut out = String::new();
    for (k, x) in t.iter().enumerate() {
        let piece = match x {
            T::Word { text, .. } => text.clone(),
            T::Str(s) => format!("'{}'", s.replace('\'', "''")),
            T::Num(n) => n.clone(),
            T::P(c) => c.to_string(),
        };
        let tight = matches!(x, T::P(',' | ')' | '.')) || k == 0 || out.ends_with(['(', '.']) || (matches!(x, T::P('(')) && matches!(t[k - 1], T::Word { .. }));
        if !tight {
            out.push(' ');
        }
        out.push_str(&piece);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn run(schema: &str, sql: &str) -> (Schema, Vec<String>, Renames) {
        let mut s = parse(schema).unwrap();
        let mut r = Renames::default();
        let w = apply(&mut s, sql, &mut r);
        (s, w, r)
    }

    const BASE: &str = "table crm_docs {\n  do_id int pk\n  do_opid int\n  do_title varchar(80)\n}\n";

    #[test]
    fn data_and_session_statements_are_quiet() {
        for sql in ["SET FOREIGN_KEY_CHECKS=0", "UPDATE crm_docs SET do_title = 'x' WHERE do_id = 1", "insert into crm_docs values (1)", "DELETE FROM crm_docs", "SELECT 1"] {
            let (s, w, _) = run(BASE, sql);
            assert!(w.is_empty(), "{sql}: {w:?}");
            assert_eq!(s, parse(BASE).unwrap());
        }
    }

    #[test]
    fn mysql_modify_change_add_drop() {
        let (s, w, r) = run(
            BASE,
            "ALTER TABLE `crm_docs` MODIFY `do_opid` VARCHAR(36) COLLATE utf8mb4_bin NULL DEFAULT NULL;
             ALTER TABLE crm_docs CHANGE do_title do_name VARCHAR(120) NOT NULL DEFAULT '' COMMENT 'Name', ADD COLUMN do_status ENUM('new','done') NOT NULL DEFAULT 'new' AFTER do_id, ADD do_flag TINYINT(1) NOT NULL DEFAULT 0;
             ALTER TABLE crm_docs ADD INDEX docs_status (do_status), ADD CONSTRAINT fk_x FOREIGN KEY (do_opid) REFERENCES crm_docs (do_id) ON DELETE CASCADE, ALGORITHM=INPLACE",
        );
        let t = s.table("crm_docs").unwrap();
        let opid = t.column("do_opid").unwrap();
        assert_eq!((opid.data_type.clone(), opid.nullable, opid.default.clone()), (DataType::Varchar(36), true, None));
        let name = t.column("do_name").unwrap();
        assert_eq!((name.data_type.clone(), name.nullable, name.comment.as_deref()), (DataType::Varchar(120), false, Some("Name")));
        assert_eq!(r.columns, vec![("crm_docs".to_string(), "do_title".to_string(), "do_name".to_string())]);
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["do_id", "do_status", "do_opid", "do_name", "do_flag"]);
        assert_eq!(t.column("do_flag").unwrap().data_type, DataType::Bool);
        assert_eq!(t.indexes[0].name, "docs_status");
        assert_eq!(t.column("do_opid").unwrap().references.as_ref().unwrap().on_delete, RefAction::Cascade);
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("raw SQL partly applied: ALTER TABLE crm_docs ADD INDEX") && w[0].ends_with("(not understood: ALGORITHM = INPLACE)"), "{w:?}");
    }

    #[test]
    fn drops_renames_and_postgres_alters() {
        let (s, w, r) = run(
            BASE,
            "ALTER TABLE crm_docs DROP COLUMN do_title, RENAME COLUMN do_opid TO do_op;
             ALTER TABLE crm_docs ALTER COLUMN do_op TYPE bigint USING do_op::bigint;
             ALTER TABLE crm_docs ALTER COLUMN do_op SET NOT NULL;
             CREATE UNIQUE INDEX docs_op ON crm_docs (do_op);
             RENAME TABLE crm_docs TO docs;",
        );
        assert!(w.is_empty(), "{w:?}");
        let t = s.table("docs").unwrap();
        assert!(t.column("do_title").is_none());
        let op = t.column("do_op").unwrap();
        assert_eq!((op.data_type.clone(), op.nullable), (DataType::BigInt, false));
        assert!(t.indexes.iter().any(|i| i.name == "docs_op" && i.unique));
        assert_eq!(r.tables, vec![("crm_docs".to_string(), "docs".to_string())]);
        let (s, w, _) = run(BASE, "DROP INDEX nope ON crm_docs; DROP TABLE IF EXISTS crm_docs");
        assert!(s.tables.is_empty());
        assert_eq!(w, vec!["raw SQL not applied: DROP INDEX nope ON crm_docs".to_string()]);
    }

    #[test]
    fn create_table_and_what_stays_unknown() {
        let (s, w, _) = run(
            "",
            "CREATE TABLE IF NOT EXISTS `logs` (`id` bigint unsigned NOT NULL AUTO_INCREMENT, `msg` longtext, `at` timestamp NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY (`id`), KEY `logs_at` (`at`)) ENGINE=InnoDB",
        );
        assert!(w.is_empty(), "{w:?}");
        let t = s.table("logs").unwrap();
        let id = t.column("id").unwrap();
        assert!(id.primary_key && id.auto_increment && id.data_type == DataType::BigInt);
        assert_eq!(t.column("msg").unwrap().data_type, DataType::Text);
        assert_eq!(t.column("at").unwrap().default, Some(DefaultValue::Now));
        assert_eq!(t.indexes[0].name, "logs_at");
        let (_, w, _) = run(BASE, "CREATE VIEW v AS SELECT 1; ALTER TABLE missing ADD x int");
        assert_eq!(w.len(), 2);
    }
}
