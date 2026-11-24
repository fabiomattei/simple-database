//! Laravel migrations (`database/migrations/*.php`) read without PHP: the
//! Schema builder calls in each file's `up()` are replayed onto a [`Schema`],
//! one [`Step`] per file, in file name order (Laravel's order). Raw SQL is
//! applied where it is plain DDL (`raw_sql`). What can't be modeled (other raw
//! SQL, unknown methods, values computed at run time) becomes a warning on that
//! step. Control flow is ignored: every call in `up()` counts.

use crate::schema::{Column, DataType, DefaultValue, ForeignKey, Index, RefAction, Schema, Table, TableForeignKey};
use crate::timeline::{Renames, Step};
use anyhow::{Context, Result};

mod raw_sql;
use std::path::{Path, PathBuf};

/// `database/migrations` in `start` or the nearest folder above it that has one.
pub fn find_dir(start: &Path) -> Option<PathBuf> {
    start.ancestors().map(|d| d.join("database").join("migrations")).find(|d| d.is_dir())
}

/// The migrations folder `path` names: itself when it holds `.php` files, else
/// the `database/migrations` of it or of a folder above it.
pub fn resolve_dir(path: &Path) -> Option<PathBuf> {
    let has_php = std::fs::read_dir(path).ok()?.filter_map(|e| e.ok()).any(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("php")));
    if has_php && !path.join("artisan").is_file() { Some(path.to_path_buf()) } else { find_dir(path) }
}

/// Every `*.php` file of `dir`, in name order, replayed one after the other.
pub fn read_dir(dir: &Path) -> Result<Vec<Step>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("php")))
        .collect();
    files.sort_by_key(|p| p.file_name().map(|n| n.to_os_string()));
    let mut schema = Schema::default();
    let mut steps = Vec::new();
    for file in files {
        let src = std::fs::read_to_string(&file).with_context(|| format!("cannot read {}", file.display()))?;
        let mut renames = Renames::default();
        let warnings = apply(&mut schema, &src, &mut renames);
        let id = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let (date, title) = split_id(&id);
        steps.push(Step { file, id, date, title, schema: schema.clone(), renames, warnings });
    }
    Ok(steps)
}

/// `2014_10_12_000000_create_users_table` → (`2014-10-12`, `create users table`).
fn split_id(id: &str) -> (Option<String>, String) {
    let parts: Vec<&str> = id.splitn(5, '_').collect();
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    if parts.len() == 5 && digits(parts[0], 4) && digits(parts[1], 2) && digits(parts[2], 2) && digits(parts[3], 6) {
        (Some(format!("{}-{}-{}", parts[0], parts[1], parts[2])), parts[4].replace('_', " "))
    } else {
        (None, id.replace('_', " "))
    }
}

/// Replays the `up()` of one migration file onto `schema`; returns its warnings.
pub fn apply(schema: &mut Schema, src: &str, renames: &mut Renames) -> Vec<String> {
    let toks = tokenize(src);
    let mut ctx = Ctx { schema, renames, warnings: Vec::new() };
    match up_body(&toks) {
        Some(body) => ctx.run(body),
        None => ctx.warn("no up() method found".into()),
    }
    ctx.warnings
}

// ---- tokens

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A name, possibly namespaced (`\Illuminate\Support\Facades\Schema`).
    Ident(String),
    /// `$table`, without the `$`.
    Var(String),
    Str(String),
    Num(String),
    P(&'static str),
}

const PUNCT: [&str; 14] = ["?->", "...", "->", "::", "=>", "==", "!=", "&&", "||", "<=", ">=", "??", "++", "--"];

fn tokenize(src: &str) -> Vec<Tok> {
    let c: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let at = |i: usize, s: &str| s.chars().enumerate().all(|(k, ch)| c.get(i + k) == Some(&ch));
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if at(i, "<?php") {
            i += 5;
        } else if at(i, "?>") {
            i += 2;
        } else if at(i, "#[") {
            // An attribute: skip to its closing bracket.
            let mut depth = 0;
            while i < c.len() {
                match c[i] {
                    '[' => depth += 1,
                    ']' => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
        } else if ch == '#' || at(i, "//") {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if at(i, "/*") {
            i += 2;
            while i < c.len() && !at(i, "*/") {
                i += 1;
            }
            i += 2;
        } else if at(i, "<<<") {
            let (s, next) = heredoc(&c, i + 3);
            out.push(Tok::Str(s));
            i = next;
        } else if ch == '\'' || ch == '"' {
            let mut s = String::new();
            i += 1;
            while i < c.len() && c[i] != ch {
                if c[i] == '\\' && i + 1 < c.len() {
                    let e = c[i + 1];
                    match (ch, e) {
                        ('\'', '\'' | '\\') => s.push(e),
                        ('\'', _) => {
                            s.push('\\');
                            s.push(e);
                        }
                        (_, 'n') => s.push('\n'),
                        (_, 't') => s.push('\t'),
                        (_, '"' | '\\' | '$') => s.push(e),
                        _ => {
                            s.push('\\');
                            s.push(e);
                        }
                    }
                    i += 2;
                } else {
                    s.push(c[i]);
                    i += 1;
                }
            }
            i += 1;
            out.push(Tok::Str(s));
        } else if ch == '$' && c.get(i + 1).is_some_and(|n| n.is_alphabetic() || *n == '_') {
            let start = i + 1;
            i += 1;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_') {
                i += 1;
            }
            out.push(Tok::Var(c[start..i].iter().collect()));
        } else if ch.is_alphabetic() || ch == '_' || (ch == '\\' && c.get(i + 1).is_some_and(|n| n.is_alphabetic())) {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '\\') {
                i += 1;
            }
            out.push(Tok::Ident(c[start..i].iter().collect()));
        } else if ch.is_ascii_digit() {
            let start = i;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '.' || c[i] == '_') {
                i += 1;
            }
            out.push(Tok::Num(c[start..i].iter().filter(|d| **d != '_').collect()));
        } else if let Some(p) = PUNCT.iter().find(|p| at(i, p)) {
            out.push(Tok::P(p));
            i += p.chars().count();
        } else {
            const SINGLE: &str = "(){}[],;:=!.+-*/%<>?&|^~@";
            const NAMES: [&str; 25] = ["(", ")", "{", "}", "[", "]", ",", ";", ":", "=", "!", ".", "+", "-", "*", "/", "%", "<", ">", "?", "&", "|", "^", "~", "@"];
            if let Some(k) = SINGLE.find(ch) {
                out.push(Tok::P(NAMES[k]));
            }
            i += 1;
        }
    }
    out
}

/// A heredoc or nowdoc body, `i` just after `<<<`; returns it and where it ends.
fn heredoc(c: &[char], mut i: usize) -> (String, usize) {
    while i < c.len() && c[i] == ' ' {
        i += 1;
    }
    let mut label = String::new();
    while i < c.len() && c[i] != '\n' {
        if c[i].is_alphanumeric() || c[i] == '_' {
            label.push(c[i]);
        }
        i += 1;
    }
    let mut lines = Vec::new();
    let rest: String = c[(i + 1).min(c.len())..].iter().collect();
    let mut consumed = i + 1;
    for line in rest.split_inclusive('\n') {
        consumed += line.chars().count();
        let trimmed = line.trim_start();
        if !label.is_empty() && trimmed.starts_with(&label) && !trimmed[label.len()..].starts_with(|ch: char| ch.is_alphanumeric() || ch == '_') {
            consumed -= trimmed.chars().count() - label.chars().count();
            break;
        }
        lines.push(line.trim_end_matches(['\n', '\r']));
    }
    (lines.join("\n"), consumed.min(c.len()))
}

/// The tokens inside the braces of `function up(...) { ... }`.
fn up_body(t: &[Tok]) -> Option<&[Tok]> {
    let start = t.windows(3).position(|w| {
        w[0] == Tok::Ident("function".into()) && matches!(&w[1], Tok::Ident(n) if n.eq_ignore_ascii_case("up")) && w[2] == Tok::P("(")
    })?;
    let open = start + t[start..].iter().position(|x| *x == Tok::P("{"))?;
    let close = matching(t, open)?;
    Some(&t[open + 1..close])
}

/// Index of the bracket closing the one at `open`.
fn matching(t: &[Tok], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (k, x) in t.iter().enumerate().skip(open) {
        match x {
            Tok::P("(" | "[" | "{") => depth += 1,
            Tok::P(")" | "]" | "}") => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

// ---- calls and values

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Str(String),
    Num(String),
    Bool(bool),
    Null,
    Array(Vec<Val>),
    /// `User::class` → `User`.
    Class(String),
    /// `DB::raw('…')` or `new Expression('…')`: SQL.
    Raw(String),
    Closure { var: String, body: Vec<Tok> },
    /// Anything else, as source-ish text.
    Other(String),
}

impl Val {
    fn str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }

    /// A column list: one string or an array of them.
    fn names(&self) -> Vec<String> {
        match self {
            Val::Str(s) => vec![s.clone()],
            Val::Array(items) => items.iter().filter_map(|v| v.str().map(str::to_string)).collect(),
            _ => vec![],
        }
    }

    fn truthy(&self) -> Option<bool> {
        match self {
            Val::Bool(b) => Some(*b),
            Val::Num(n) => Some(n != "0"),
            Val::Null => Some(false),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Arg {
    name: Option<String>,
    val: Val,
}

#[derive(Debug, Clone)]
struct Call {
    name: String,
    args: Vec<Arg>,
}

impl Call {
    /// Argument `name:` or else the `pos`-th positional one.
    fn arg(&self, pos: usize, name: &str) -> Option<&Val> {
        self.args
            .iter()
            .find(|a| a.name.as_deref() == Some(name))
            .or_else(|| self.args.iter().filter(|a| a.name.is_none()).nth(pos))
            .map(|a| &a.val)
    }

    fn str(&self, pos: usize, name: &str) -> Option<String> {
        self.arg(pos, name).and_then(Val::str).map(str::to_string)
    }

    fn num(&self, pos: usize, name: &str) -> Option<u32> {
        match self.arg(pos, name)? {
            Val::Num(n) => n.parse().ok(),
            _ => None,
        }
    }
}

struct Parser<'a> {
    t: &'a [Tok],
    i: usize,
}

impl Parser<'_> {
    fn peek(&self, k: usize) -> Option<&Tok> {
        self.t.get(self.i + k)
    }

    fn is(&self, k: usize, p: &str) -> bool {
        matches!(self.peek(k), Some(Tok::P(x)) if *x == p)
    }

    fn ident(&self, k: usize) -> Option<&str> {
        match self.peek(k) {
            Some(Tok::Ident(s)) => Some(s),
            _ => None,
        }
    }

    /// Arguments of a call, at its `(`; leaves the cursor after the `)`.
    fn args(&mut self) -> Vec<Arg> {
        let mut out = Vec::new();
        if !self.is(0, "(") {
            return out;
        }
        self.i += 1;
        while self.i < self.t.len() && !self.is(0, ")") {
            let name = match (self.ident(0), self.is(1, ":"), self.is(1, "::")) {
                (Some(n), true, false) => {
                    let n = n.to_string();
                    self.i += 2;
                    Some(n)
                }
                _ => None,
            };
            if self.is(0, "...") {
                self.i += 1;
            }
            let val = self.value();
            out.push(Arg { name, val });
            if self.is(0, ",") {
                self.i += 1;
            } else if !self.is(0, ")") {
                self.skip_expr();
            }
        }
        self.i += 1;
        out
    }

    /// One expression; the cursor ends on the `,` `)` `]` or `;` after it.
    fn value(&mut self) -> Val {
        let start = self.i;
        let v = self.simple_value();
        if self.at_end_of_expr() {
            return v.unwrap_or(Val::Other(String::new()));
        }
        self.i = start;
        self.skip_expr();
        Val::Other(text(&self.t[start..self.i]))
    }

    fn at_end_of_expr(&self) -> bool {
        self.peek(0).is_none() || self.is(0, ",") || self.is(0, ")") || self.is(0, "]") || self.is(0, ";")
    }

    fn simple_value(&mut self) -> Option<Val> {
        let v = match self.peek(0)? {
            Tok::Str(s) => {
                let s = s.clone();
                self.i += 1;
                Val::Str(s)
            }
            Tok::Num(n) => {
                let n = n.clone();
                self.i += 1;
                Val::Num(n)
            }
            Tok::P("-") => {
                let Some(Tok::Num(n)) = self.peek(1) else { return None };
                let n = format!("-{n}");
                self.i += 2;
                Val::Num(n)
            }
            Tok::P("[") => {
                self.i += 1;
                Val::Array(self.items("]"))
            }
            Tok::Ident(id) => {
                let id = id.clone();
                let last = id.rsplit('\\').next().unwrap_or(&id).to_string();
                let lower = last.to_ascii_lowercase();
                match lower.as_str() {
                    "true" | "false" => {
                        self.i += 1;
                        Val::Bool(lower == "true")
                    }
                    "null" => {
                        self.i += 1;
                        Val::Null
                    }
                    "array" if self.is(1, "(") => {
                        self.i += 2;
                        Val::Array(self.items(")"))
                    }
                    "function" | "fn" | "static" => return self.closure(),
                    "new" => {
                        let class = self.ident(1)?.rsplit('\\').next()?.to_string();
                        self.i += 2;
                        let args = self.args();
                        match (class.as_str(), args.first().map(|a| &a.val)) {
                            ("Expression", Some(Val::Str(s))) => Val::Raw(s.clone()),
                            _ => return None,
                        }
                    }
                    _ if self.is(1, "::") && self.ident(2) == Some("class") => {
                        self.i += 3;
                        Val::Class(last)
                    }
                    _ if self.is(1, "::") && self.ident(2).is_some_and(|m| m.eq_ignore_ascii_case("raw")) && last == "DB" => {
                        self.i += 3;
                        match self.args().first().map(|a| a.val.clone()) {
                            Some(Val::Str(s)) => Val::Raw(s),
                            _ => return None,
                        }
                    }
                    _ => return None,
                }
            }
            _ => return None,
        };
        Some(v)
    }

    /// Items of an array up to `close`, which it consumes. Keys are dropped.
    fn items(&mut self, close: &str) -> Vec<Val> {
        let mut out = Vec::new();
        while self.i < self.t.len() && !self.is(0, close) {
            let mut v = self.value();
            if self.is(0, "=>") {
                self.i += 1;
                v = self.value();
            }
            out.push(v);
            if self.is(0, ",") {
                self.i += 1;
            } else if !self.is(0, close) {
                self.skip_expr();
                if !self.is(0, close) {
                    self.i += 1;
                }
            }
        }
        self.i += 1;
        out
    }

    /// `function (Blueprint $table) use (...) { … }` or `fn ($t) => …`.
    fn closure(&mut self) -> Option<Val> {
        if self.ident(0) == Some("static") {
            self.i += 1;
        }
        let arrow = self.ident(0) == Some("fn");
        self.i += 1;
        if !self.is(0, "(") {
            return None;
        }
        let close = matching(self.t, self.i)?;
        let var = self.t[self.i..close].iter().find_map(|x| match x {
            Tok::Var(v) => Some(v.clone()),
            _ => None,
        })?;
        self.i = close + 1;
        if arrow {
            while self.i < self.t.len() && !self.is(0, "=>") {
                self.i += 1;
            }
            self.i += 1;
            let start = self.i;
            self.skip_expr();
            return Some(Val::Closure { var, body: self.t[start..self.i].to_vec() });
        }
        while self.i < self.t.len() && !self.is(0, "{") {
            self.i += 1;
        }
        let close = matching(self.t, self.i)?;
        let body = self.t[self.i + 1..close].to_vec();
        self.i = close + 1;
        Some(Val::Closure { var, body })
    }

    /// Moves to the end of the current expression (depth 0 `,` `)` `]` `;`).
    fn skip_expr(&mut self) {
        let mut depth = 0i32;
        while let Some(x) = self.peek(0) {
            match x {
                Tok::P("(" | "[" | "{") => depth += 1,
                Tok::P(")" | "]" | "}") if depth == 0 => return,
                Tok::P(")" | "]" | "}") => depth -= 1,
                Tok::P("," | ";") if depth == 0 => return,
                _ => {}
            }
            self.i += 1;
        }
    }

    /// `->a(…)->b(…)…` from the cursor; properties count as calls without arguments.
    fn chain(&mut self) -> Vec<Call> {
        let mut calls = Vec::new();
        while self.is(0, "->") || self.is(0, "?->") || self.is(0, "::") {
            self.i += 1;
            let Some(name) = self.ident(0).map(str::to_string) else { break };
            self.i += 1;
            let args = self.args();
            calls.push(Call { name, args });
        }
        calls
    }
}

/// Tokens as rough PHP text, for warnings.
fn text(t: &[Tok]) -> String {
    let mut s = String::new();
    for x in t {
        let piece = match x {
            Tok::Ident(i) => i.clone(),
            Tok::Var(v) => format!("${v}"),
            Tok::Str(v) => format!("'{v}'"),
            Tok::Num(n) => n.clone(),
            Tok::P(p) => p.to_string(),
        };
        let glue = matches!(x, Tok::P("->" | "::" | "(" | ")" | "," | "?->")) || s.ends_with(['(', '>', ':']) || s.is_empty();
        if !glue {
            s.push(' ');
        }
        s.push_str(&piece);
    }
    s
}

fn short(s: &str) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 90 { format!("{}…", one.chars().take(90).collect::<String>()) } else { one }
}

// ---- replaying

struct Ctx<'a> {
    schema: &'a mut Schema,
    renames: &'a mut Renames,
    warnings: Vec<String>,
}

impl Ctx<'_> {
    fn warn(&mut self, w: String) {
        if !self.warnings.contains(&w) {
            self.warnings.push(w);
        }
    }

    /// Every `Schema::…` and `DB::…` call in a body, in order.
    fn run(&mut self, body: &[Tok]) {
        let mut p = Parser { t: body, i: 0 };
        while p.i < body.len() {
            let facade = p.ident(0).map(|id| id.rsplit('\\').next().unwrap_or(id).to_string());
            match facade.as_deref() {
                Some("Schema") if p.is(1, "::") => {
                    p.i += 1;
                    let calls = p.chain();
                    self.schema_calls(&calls);
                }
                Some("DB") if p.is(1, "::") => {
                    p.i += 1;
                    let calls = p.chain();
                    // DDL is applied; reads, writes of rows and session settings change nothing here.
                    if let Some(c) = calls.iter().find(|c| matches!(c.name.as_str(), "statement" | "unprepared")) {
                        match c.arg(0, "query") {
                            Some(Val::Str(sql)) => {
                                for w in raw_sql::apply(self.schema, sql, self.renames) {
                                    self.warn(w);
                                }
                            }
                            Some(Val::Raw(sql)) => {
                                for w in raw_sql::apply(self.schema, sql, self.renames) {
                                    self.warn(w);
                                }
                            }
                            other => {
                                let text = match other {
                                    Some(Val::Other(t)) => t.clone(),
                                    _ => String::new(),
                                };
                                self.warn(format!("raw SQL built at run time not applied: {}", short(&text)));
                            }
                        }
                    }
                }
                _ => p.i += 1,
            }
        }
    }

    fn schema_calls(&mut self, calls: &[Call]) {
        let Some(c) = calls.iter().find(|c| !matches!(c.name.as_str(), "connection")) else { return };
        let table = c.str(0, "table");
        match c.name.as_str() {
            "create" | "table" => {
                let Some(name) = table else { return self.warn(format!("Schema::{} with a table name computed at run time", c.name)) };
                let Some(Val::Closure { var, body }) = c.arg(1, "callback") else { return };
                if c.name == "create" {
                    if self.schema.table(&name).is_some() {
                        self.warn(format!("{name} is created again; the earlier one is replaced"));
                        self.schema.drop_table(&name);
                    }
                    self.schema.tables.push(Table::new(name.clone()));
                } else if self.schema.table(&name).is_none() {
                    return self.warn(format!("Schema::table on {name}, which doesn't exist yet"));
                }
                let (var, body) = (var.clone(), body.clone());
                self.blueprint(name, &var, &body);
            }
            "rename" => {
                if let (Some(from), Some(to)) = (table, c.str(1, "to")) {
                    self.rename_table(&from, &to);
                }
            }
            "drop" | "dropIfExists" => {
                if let Some(name) = table {
                    if self.schema.table(&name).is_none() && c.name == "drop" {
                        self.warn(format!("Schema::drop on {name}, which doesn't exist"));
                    }
                    self.schema.drop_table(&name);
                }
            }
            "dropColumns" => {
                if let (Some(name), Some(cols)) = (table, c.arg(1, "columns")) {
                    for col in cols.names() {
                        self.schema.drop_column(&name, &col);
                    }
                }
            }
            "withoutForeignKeyConstraints" => {
                if let Some(Val::Closure { body, .. }) = c.arg(0, "callback") {
                    let body = body.clone();
                    self.run(&body);
                }
            }
            "dropAllTables" => self.schema.tables.clear(),
            n if n.starts_with("has") || n.starts_with("get") || n.ends_with("ForeignKeyConstraints") || n == "defaultStringLength" || n == "morphUsingUuids" || n == "morphUsingUlids" => {}
            n => self.warn(format!("Schema::{n} not modeled")),
        }
    }

    fn rename_table(&mut self, from: &str, to: &str) {
        if self.schema.table(from).is_none() {
            return self.warn(format!("rename of {from}, which doesn't exist"));
        }
        self.schema.rename_table(from, to);
        self.renames.table(from, to);
    }

    fn table_mut(&mut self, name: &str) -> &mut Table {
        self.schema.tables.iter_mut().find(|t| t.name == name).expect("table exists")
    }

    /// The commands on `$var` inside a `Schema::create` / `table` closure.
    fn blueprint(&mut self, mut table: String, var: &str, body: &[Tok]) {
        let mut p = Parser { t: body, i: 0 };
        while p.i < body.len() {
            if matches!(p.peek(0), Some(Tok::Var(v)) if v == var) && p.is(1, "->") {
                p.i += 1;
                let calls = p.chain();
                if !calls.is_empty() {
                    self.command(&mut table, &calls);
                }
            } else {
                p.i += 1;
            }
        }
    }

    fn command(&mut self, table: &mut String, calls: &[Call]) {
        let (head, mods) = calls.split_first().unwrap();
        let t = table.clone();
        let col_arg = |k: usize| head.str(k, "column");
        if let Some(cols) = column_defs(head, &t) {
            return self.add_columns(&t, cols, head, mods);
        }
        match head.name.as_str() {
            "primary" => {
                let cols = head.arg(0, "columns").map(Val::names).unwrap_or_default();
                let tb = self.table_mut(&t);
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
            }
            "unique" | "index" | "fullText" | "fulltext" | "spatialIndex" => {
                let cols = head.arg(0, "columns").map(Val::names).unwrap_or_default();
                if cols.is_empty() {
                    return self.warn(format!("{t}: {}() with columns computed at run time", head.name));
                }
                let kind = if head.name == "unique" { "unique" } else { "index" };
                let name = head.str(1, "name").unwrap_or_else(|| index_name(&t, &cols, &head.name.to_ascii_lowercase()));
                self.add_index(&t, Index::new(name, cols, kind == "unique"));
            }
            "rawIndex" => {
                if let (Some(expr), Some(name)) = (head.str(0, "expression"), head.str(1, "name")) {
                    self.add_index(&t, Index::new(name, vec![Index::expression_part(&expr)], false));
                }
            }
            "foreign" => {
                let cols = head.arg(0, "columns").map(Val::names).unwrap_or_default();
                let fk = foreign_key(&cols, mods, None);
                match fk {
                    Some(fk) => self.add_foreign_key(&t, fk),
                    None => self.warn(format!("{t}: foreign key on ({}) without a table", cols.join(", "))),
                }
            }
            "dropColumn" | "dropColumns" => {
                let cols: Vec<String> = head.args.iter().flat_map(|a| a.val.names()).collect();
                for c in cols {
                    if self.schema.table(&t).and_then(|tb| tb.column(&c)).is_none() {
                        self.warn(format!("{t}: dropColumn {c}, which doesn't exist"));
                    }
                    self.schema.drop_column(&t, &c);
                }
            }
            "renameColumn" => {
                if let (Some(from), Some(to)) = (head.str(0, "from"), head.str(1, "to")) {
                    if self.schema.table(&t).and_then(|tb| tb.column(&from)).is_none() {
                        return self.warn(format!("{t}: renameColumn {from}, which doesn't exist"));
                    }
                    self.schema.rename_column(&t, &from, &to);
                    self.renames.column(&t, &from, &to);
                }
            }
            "dropForeign" | "dropForeignIdFor" | "dropConstrainedForeignId" | "dropConstrainedForeignIdFor" => {
                let cols: Vec<String> = match head.arg(0, "index") {
                    Some(Val::Array(_)) => head.arg(0, "index").unwrap().names(),
                    Some(Val::Str(s)) if head.name == "dropConstrainedForeignId" => vec![s.clone()],
                    Some(Val::Str(name)) => self.columns_of_foreign(&t, name),
                    Some(Val::Class(model)) => vec![format!("{}_id", snake(model))],
                    _ => vec![],
                };
                if cols.is_empty() {
                    return self.warn(format!("{t}: {}() of a foreign key it can't find", head.name));
                }
                self.drop_foreign(&t, &cols);
                if head.name.starts_with("dropConstrained") {
                    for c in &cols {
                        self.schema.drop_column(&t, c);
                    }
                }
            }
            "dropIndex" | "dropUnique" | "dropFullText" | "dropSpatialIndex" => {
                let suffix = match head.name.as_str() {
                    "dropUnique" => "unique",
                    "dropFullText" => "fulltext",
                    "dropSpatialIndex" => "spatialindex",
                    _ => "index",
                };
                let name = match head.arg(0, "index") {
                    Some(Val::Str(s)) => s.clone(),
                    Some(v @ Val::Array(_)) => index_name(&t, &v.names(), suffix),
                    _ => return self.warn(format!("{t}: {}() with a name computed at run time", head.name)),
                };
                let tb = self.table_mut(&t);
                let before = tb.indexes.len();
                tb.indexes.retain(|i| i.name != name);
                if tb.indexes.len() == before {
                    self.warn(format!("{t}: {}({name}), which doesn't exist", head.name));
                }
            }
            "renameIndex" => {
                if let (Some(from), Some(to)) = (head.str(0, "from"), head.str(1, "to"))
                    && let Some(i) = self.table_mut(&t).indexes.iter_mut().find(|i| i.name == from)
                {
                    i.name = to;
                }
            }
            "dropPrimary" => {
                let tb = self.table_mut(&t);
                tb.composite_pk.clear();
                for c in &mut tb.columns {
                    c.primary_key = false;
                    c.auto_increment = false;
                }
            }
            "dropTimestamps" | "dropTimestampsTz" => {
                self.schema.drop_column(&t, "created_at");
                self.schema.drop_column(&t, "updated_at");
            }
            "dropSoftDeletes" | "dropSoftDeletesTz" => self.schema.drop_column(&t, &col_arg(0).unwrap_or_else(|| "deleted_at".into())),
            "dropRememberToken" => self.schema.drop_column(&t, "remember_token"),
            "dropMorphs" => {
                if let Some(name) = head.str(0, "name") {
                    self.table_mut(&t).indexes.retain(|i| i.name != index_name(&t, &[format!("{name}_type"), format!("{name}_id")], "index"));
                    self.schema.drop_column(&t, &format!("{name}_type"));
                    self.schema.drop_column(&t, &format!("{name}_id"));
                }
            }
            "rename" => {
                if let Some(to) = head.str(0, "to") {
                    self.rename_table(&t, &to);
                    *table = to;
                }
            }
            "comment" => self.table_mut(&t).comment = head.str(0, "comment").filter(|c| !c.is_empty()),
            "engine" | "charset" | "collation" | "temporary" | "after" => {}
            n if head.args.first().is_some_and(|a| a.val.str().is_none()) && head.args.len() <= 3 => {
                self.warn(format!("{t}: ->{n}() with a column name computed at run time"))
            }
            n => self.warn(format!("{t}: ->{n}() not modeled")),
        }
    }

    /// Adds (or with `->change()`, redefines) the columns of one definition.
    fn add_columns(&mut self, t: &str, defs: ColumnDefs, head: &Call, mods: &[Call]) {
        let ColumnDefs { mut columns, target, indexes } = defs;
        let change = mods.iter().any(|m| m.name == "change");
        let mut extra = indexes;
        if let Some(c) = columns.last_mut() {
            let mut refs: Option<ForeignKey> = None;
            for m in mods {
                match m.name.as_str() {
                    "nullable" => c.nullable = m.arg(0, "value").and_then(Val::truthy).unwrap_or(true),
                    "default" => match m.arg(0, "value").map(|v| default_value(v, &c.data_type)) {
                        Some(Ok(d)) => c.default = d,
                        Some(Err(text)) => self.warn(format!("{t}.{}: default {} not modeled", c.name, short(&text))),
                        None => {}
                    },
                    "useCurrent" => c.default = Some(DefaultValue::Now),
                    "unique" => extra.push(Index::new(m.str(0, "indexName").unwrap_or_else(|| index_name(t, std::slice::from_ref(&c.name), "unique")), vec![c.name.clone()], true)),
                    "index" | "fullText" | "fulltext" | "spatialIndex" => {
                        extra.push(Index::new(m.str(0, "indexName").unwrap_or_else(|| index_name(t, std::slice::from_ref(&c.name), &m.name.to_ascii_lowercase())), vec![c.name.clone()], false))
                    }
                    "primary" => {
                        c.primary_key = true;
                        c.nullable = false;
                    }
                    "autoIncrement" => {
                        c.auto_increment = true;
                        c.primary_key = true;
                        c.nullable = false;
                    }
                    "comment" => c.comment = m.str(0, "comment").filter(|s| !s.is_empty()),
                    "constrained" => {
                        let table = m.str(0, "table").or_else(|| target.clone()).unwrap_or_else(|| guess_table(&c.name, m.str(1, "column").as_deref().unwrap_or("id")));
                        let column = m.str(1, "column").unwrap_or_else(|| "id".into());
                        refs = Some(ForeignKey { table, column, on_delete: RefAction::NoAction, on_update: RefAction::NoAction });
                    }
                    "references" => {
                        let column = m.arg(0, "columns").map(Val::names).and_then(|v| v.into_iter().next()).unwrap_or_else(|| "id".into());
                        let fk = refs.get_or_insert(ForeignKey { table: String::new(), column: String::new(), on_delete: RefAction::NoAction, on_update: RefAction::NoAction });
                        fk.column = column;
                    }
                    "on" => {
                        if let Some(table) = m.str(0, "table") {
                            let fk = refs.get_or_insert(ForeignKey { table: String::new(), column: "id".into(), on_delete: RefAction::NoAction, on_update: RefAction::NoAction });
                            fk.table = table;
                        }
                    }
                    other => {
                        if let Some((delete, action)) = ref_action(other, m) {
                            if let Some(fk) = refs.as_mut() {
                                if delete { fk.on_delete = action } else { fk.on_update = action }
                            }
                        } else if !IGNORED_MODIFIERS.contains(&other) {
                            self.warn(format!("{t}.{}: ->{other}() not modeled", c.name));
                        }
                    }
                }
            }
            if let Some(fk) = refs {
                if fk.table.is_empty() {
                    self.warn(format!("{t}.{}: foreign key without a table", c.name));
                } else {
                    c.references = Some(fk);
                }
            }
            if let Some(Val::Bool(true)) = head.arg(1, "autoIncrement").filter(|_| head.name.ends_with("nteger")) {
                c.auto_increment = true;
                c.primary_key = true;
                c.nullable = false;
            }
        }
        for c in columns {
            let tb = self.table_mut(t);
            match tb.columns.iter_mut().find(|x| x.name == c.name) {
                Some(old) if change => {
                    old.data_type = c.data_type;
                    old.nullable = c.nullable && !old.primary_key;
                    old.default = c.default;
                    old.comment = c.comment;
                    old.auto_increment |= c.auto_increment;
                    old.primary_key |= c.primary_key;
                    if c.references.is_some() {
                        old.references = c.references;
                    }
                }
                Some(old) => {
                    *old = c;
                    let name = old.name.clone();
                    self.warn(format!("{t}.{name} is added again; the earlier one is replaced"));
                }
                None if change => {
                    let name = c.name.clone();
                    self.warn(format!("{t}.{name}: change() of a column that doesn't exist"));
                }
                None => tb.columns.push(c),
            }
        }
        for i in extra {
            self.add_index(t, i);
        }
    }

    fn add_index(&mut self, t: &str, i: Index) {
        let tb = self.table_mut(t);
        tb.indexes.retain(|x| x.name != i.name);
        tb.indexes.push(i);
    }

    fn add_foreign_key(&mut self, t: &str, fk: TableForeignKey) {
        let tb = self.table_mut(t);
        if let [col] = fk.columns.as_slice()
            && let Some(c) = tb.columns.iter_mut().find(|c| c.name == *col)
        {
            c.references = Some(ForeignKey { table: fk.table, column: fk.ref_columns.first().cloned().unwrap_or_else(|| "id".into()), on_delete: fk.on_delete, on_update: fk.on_update });
            return;
        }
        tb.foreign_keys.retain(|x| x.columns != fk.columns);
        tb.foreign_keys.push(fk);
    }

    /// The columns of the foreign key Laravel names `name` (`posts_user_id_foreign`).
    fn columns_of_foreign(&self, t: &str, name: &str) -> Vec<String> {
        let Some(tb) = self.schema.table(t) else { return vec![] };
        tb.all_foreign_keys().into_iter().map(|fk| fk.columns).find(|cols| index_name(t, cols, "foreign") == name).unwrap_or_default()
    }

    fn drop_foreign(&mut self, t: &str, cols: &[String]) {
        let tb = self.table_mut(t);
        if let [col] = cols
            && let Some(c) = tb.columns.iter_mut().find(|c| c.name == *col)
        {
            c.references = None;
        }
        tb.foreign_keys.retain(|fk| fk.columns != cols);
    }
}

/// Column modifiers that change nothing sdb models.
const IGNORED_MODIFIERS: [&str; 22] = [
    "change", "unsigned", "after", "first", "charset", "collation", "useCurrentOnUpdate", "storedAs", "virtualAs", "generatedAs", "always", "invisible", "from",
    "startingValue", "isGeometry", "projection", "persisted", "indexName", "lock", "type", "srid", "total",
];

/// `cascadeOnDelete`, `onDelete('set null')`, …: (on delete?, action).
fn ref_action(name: &str, m: &Call) -> Option<(bool, RefAction)> {
    let fixed = |s: &str| match s {
        "cascade" => Some(RefAction::Cascade),
        "restrict" => Some(RefAction::Restrict),
        "null" => Some(RefAction::SetNull),
        "noAction" => Some(RefAction::NoAction),
        _ => None,
    };
    if let Some(rest) = name.strip_suffix("OnDelete") {
        return Some((true, fixed(rest)?));
    }
    if let Some(rest) = name.strip_suffix("OnUpdate") {
        return Some((false, fixed(rest)?));
    }
    let delete = match name {
        "onDelete" => true,
        "onUpdate" => false,
        _ => return None,
    };
    Some((delete, RefAction::parse(&m.str(0, "action")?).unwrap_or(RefAction::NoAction)))
}

/// A `foreign(...)` command's key: `->references(...)->on(...)` and actions.
fn foreign_key(cols: &[String], mods: &[Call], table: Option<String>) -> Option<TableForeignKey> {
    let mut fk = TableForeignKey { columns: cols.to_vec(), table: table.unwrap_or_default(), ref_columns: vec!["id".into()], on_delete: RefAction::NoAction, on_update: RefAction::NoAction };
    for m in mods {
        match m.name.as_str() {
            "references" => fk.ref_columns = m.arg(0, "columns").map(Val::names).unwrap_or_default(),
            "on" => fk.table = m.str(0, "table").unwrap_or_default(),
            other => {
                if let Some((delete, action)) = ref_action(other, m) {
                    if delete { fk.on_delete = action } else { fk.on_update = action }
                }
            }
        }
    }
    (!fk.table.is_empty() && !cols.is_empty()).then_some(fk)
}

/// Laravel's default index name: `users_email_unique`.
fn index_name(table: &str, cols: &[String], kind: &str) -> String {
    format!("{}_{}_{}", crate::schema::unqualified(table), cols.join("_"), kind).to_ascii_lowercase().replace(['-', '.'], "_")
}

/// The columns (and indexes) a column-type method defines.
struct ColumnDefs {
    columns: Vec<Column>,
    /// `foreignIdFor`'s model table, for `constrained()`.
    target: Option<String>,
    indexes: Vec<Index>,
}

fn column(name: &str, t: DataType) -> Column {
    let mut c = Column::new(name, t);
    c.nullable = false;
    c
}

fn key(name: &str, t: DataType) -> Column {
    let mut c = column(name, t);
    c.primary_key = true;
    c.auto_increment = true;
    c
}

/// `None` if `head` isn't a column-type method.
fn column_defs(head: &Call, table: &str) -> Option<ColumnDefs> {
    let name = head.str(0, "column");
    let named = |t: DataType| name.as_deref().map(|n| vec![column(n, t)]);
    let one = |cols: Option<Vec<Column>>| cols.map(|columns| ColumnDefs { columns, target: None, indexes: vec![] });
    let len = |k: usize, d: u32| head.num(k, "length").unwrap_or(d);
    let defs = match head.name.as_str() {
        "id" => one(Some(vec![key(name.as_deref().unwrap_or("id"), DataType::BigInt)])),
        "bigIncrements" => one(name.map(|n| vec![key(&n, DataType::BigInt)])),
        "increments" | "mediumIncrements" => one(name.map(|n| vec![key(&n, DataType::Int)])),
        "smallIncrements" | "tinyIncrements" => one(name.map(|n| vec![key(&n, DataType::SmallInt)])),
        "integer" | "mediumInteger" | "unsignedInteger" | "unsignedMediumInteger" => one(named(DataType::Int)),
        "bigInteger" | "unsignedBigInteger" => one(named(DataType::BigInt)),
        "smallInteger" | "tinyInteger" | "unsignedSmallInteger" | "unsignedTinyInteger" | "year" => one(named(DataType::SmallInt)),
        "foreignId" => one(named(DataType::BigInt)),
        "foreignUuid" => one(named(DataType::Uuid)),
        "foreignUlid" => one(named(DataType::Varchar(26))),
        "foreignIdFor" => {
            let model = match head.arg(0, "model") {
                Some(Val::Class(m)) => snake(m),
                Some(Val::Str(m)) => snake(m.rsplit('\\').next().unwrap_or(m)),
                _ => return None,
            };
            let col = head.str(1, "column").unwrap_or_else(|| format!("{model}_id"));
            Some(ColumnDefs { columns: vec![column(&col, DataType::BigInt)], target: Some(plural(&model)), indexes: vec![] })
        }
        "string" => one(named(DataType::Varchar(len(1, 255)))),
        "char" => one(named(DataType::Varchar(len(1, 255)))),
        "text" | "tinyText" | "mediumText" | "longText" => one(named(DataType::Text)),
        "boolean" => one(named(DataType::Bool)),
        "decimal" | "unsignedDecimal" => one(named(DataType::Decimal(head.num(1, "total").unwrap_or(8), head.num(2, "places").unwrap_or(2)))),
        "float" | "double" | "unsignedFloat" | "unsignedDouble" => one(named(DataType::Real)),
        "date" => one(named(DataType::Date)),
        "dateTime" | "dateTimeTz" | "timestamp" | "timestampTz" => one(named(DataType::Timestamp)),
        "time" | "timeTz" => one(named(DataType::Time)),
        "json" | "jsonb" => one(named(DataType::Json)),
        "uuid" => one(named(DataType::Uuid)),
        "ulid" => one(named(DataType::Varchar(26))),
        "binary" => one(named(DataType::Blob)),
        "ipAddress" => one(Some(vec![column(name.as_deref().unwrap_or("ip_address"), DataType::Varchar(45))])),
        "macAddress" => one(Some(vec![column(name.as_deref().unwrap_or("mac_address"), DataType::Varchar(17))])),
        "enum" => {
            let values = head.arg(1, "allowed").map(Val::names).unwrap_or_default();
            one(named(DataType::Enum(values)))
        }
        "set" => one(named(DataType::Text)),
        "geometry" | "geography" | "point" | "lineString" | "polygon" | "multiPoint" | "multiLineString" | "multiPolygon" | "geometryCollection" => {
            one(named(DataType::Custom(head.name.to_ascii_lowercase())))
        }
        "vector" => one(named(DataType::Custom(format!("vector({})", head.num(1, "dimensions").unwrap_or(0))))),
        "timestamps" | "timestampsTz" | "nullableTimestamps" | "datetimes" => one(Some(
            ["created_at", "updated_at"]
                .iter()
                .map(|n| {
                    let mut c = column(n, DataType::Timestamp);
                    c.nullable = true;
                    c
                })
                .collect(),
        )),
        "softDeletes" | "softDeletesTz" | "softDeletesDatetime" => {
            let mut c = column(name.as_deref().unwrap_or("deleted_at"), DataType::Timestamp);
            c.nullable = true;
            one(Some(vec![c]))
        }
        "rememberToken" => {
            let mut c = column("remember_token", DataType::Varchar(100));
            c.nullable = true;
            one(Some(vec![c]))
        }
        "morphs" | "nullableMorphs" | "uuidMorphs" | "nullableUuidMorphs" | "ulidMorphs" | "nullableUlidMorphs" | "numericMorphs" | "nullableNumericMorphs" => {
            let n = name?;
            let id_type = if head.name.contains("Uuid") {
                DataType::Uuid
            } else if head.name.contains("Ulid") {
                DataType::Varchar(26)
            } else {
                DataType::BigInt
            };
            let nullable = head.name.starts_with("nullable");
            let mut cols = vec![column(&format!("{n}_type"), DataType::Varchar(255)), column(&format!("{n}_id"), id_type)];
            for c in &mut cols {
                c.nullable = nullable;
            }
            let parts = vec![format!("{n}_type"), format!("{n}_id")];
            let index = head.str(1, "indexName").unwrap_or_else(|| index_name(table, &parts, "index"));
            Some(ColumnDefs { columns: cols, target: None, indexes: vec![Index::new(index, parts, false)] })
        }
        _ => None,
    };
    defs.filter(|d| !d.columns.is_empty())
}

/// A `default(...)` value as sdb's, or the source text when it can't be one.
fn default_value(v: &Val, t: &DataType) -> Result<Option<DefaultValue>, String> {
    Ok(Some(match v {
        Val::Null => return Ok(None),
        Val::Str(s) => DefaultValue::String(s.clone()),
        Val::Num(n) if *t == DataType::Bool && (n == "0" || n == "1") => DefaultValue::Bool(n == "1"),
        Val::Num(n) => DefaultValue::Number(n.clone()),
        Val::Bool(b) if *t == DataType::Bool => DefaultValue::Bool(*b),
        Val::Bool(b) => DefaultValue::Number(if *b { "1" } else { "0" }.into()),
        Val::Raw(e) if e.eq_ignore_ascii_case("current_timestamp") || e.eq_ignore_ascii_case("now()") => DefaultValue::Now,
        Val::Raw(e) => DefaultValue::Expr(e.clone()),
        Val::Array(_) => DefaultValue::String("[]".into()),
        Val::Other(s) | Val::Class(s) => return Err(s.clone()),
        Val::Closure { .. } => return Err("a closure".into()),
    }))
}

/// `user_id` → `users`, as `constrained()` guesses: the plural of the name
/// before `_<column>`.
fn guess_table(column: &str, key: &str) -> String {
    let base = column.strip_suffix(&format!("_{key}")).unwrap_or(column);
    plural(base)
}

/// `BlogPost` → `blog_post`.
fn snake(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// English plural of the last word of a snake_case name, as Laravel's
/// `Str::plural` gives it for common words.
pub fn plural(name: &str) -> String {
    let (head, word) = match name.rfind('_') {
        Some(i) => (&name[..=i], &name[i + 1..]),
        None => ("", name),
    };
    const IRREGULAR: [(&str, &str); 10] = [
        ("person", "people"),
        ("child", "children"),
        ("man", "men"),
        ("woman", "women"),
        ("mouse", "mice"),
        ("goose", "geese"),
        ("tooth", "teeth"),
        ("foot", "feet"),
        ("ox", "oxen"),
        ("leaf", "leaves"),
    ];
    const SAME: [&str; 12] = ["data", "equipment", "information", "rice", "money", "species", "series", "fish", "sheep", "deer", "news", "media"];
    let lower = word.to_ascii_lowercase();
    let p = if let Some((_, pl)) = IRREGULAR.iter().find(|(s, _)| *s == lower) {
        pl.to_string()
    } else if SAME.contains(&lower.as_str()) || lower.is_empty() {
        word.to_string()
    } else if lower.ends_with('y') && !matches!(lower.chars().rev().nth(1), Some('a' | 'e' | 'i' | 'o' | 'u')) {
        format!("{}ies", &word[..word.len() - 1])
    } else if ["s", "x", "z", "ch", "sh"].iter().any(|e| lower.ends_with(e)) {
        format!("{word}es")
    } else {
        format!("{word}s")
    };
    format!("{head}{p}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::{Mark, changes};

    const USERS: &str = r#"<?php

use Illuminate\Database\Migrations\Migration;
use Illuminate\Database\Schema\Blueprint;
use Illuminate\Support\Facades\Schema;

return new class extends Migration
{
    /**
     * Run the migrations.
     */
    public function up(): void
    {
        Schema::create('users', function (Blueprint $table) {
            $table->id();
            $table->string('name');
            $table->string('email')->unique();
            $table->timestamp('email_verified_at')->nullable();
            $table->string('password');
            $table->rememberToken();
            $table->timestamps();
        });

        Schema::create('password_reset_tokens', function (Blueprint $table) {
            $table->string('email')->primary();
            $table->string('token');
            $table->timestamp('created_at')->nullable();
        });
    }

    public function down(): void
    {
        Schema::dropIfExists('users');
        Schema::dropIfExists('password_reset_tokens');
    }
};
"#;

    const POSTS: &str = r#"<?php
return new class extends Migration {
    public function up(): void
    {
        Schema::create('posts', function (Blueprint $table) {
            $table->id();
            $table->foreignId('user_id')->constrained()->cascadeOnDelete();
            $table->foreignIdFor(Category::class)->nullable()->constrained();
            $table->string('title', 120);
            $table->text('body')->nullable(); // comment
            $table->boolean('published')->default(false);
            $table->decimal('price', 10, 2)->default(0);
            $table->enum('status', ['draft', 'live'])->default('draft');
            $table->unsignedBigInteger('editor_id')->nullable();
            $table->foreign('editor_id')->references('id')->on('users')->nullOnDelete();
            $table->morphs('taggable');
            $table->index(['user_id', 'published']);
            $table->softDeletes();
        });
        DB::statement('CREATE EXTENSION IF NOT EXISTS pg_trgm');
        DB::statement("SET FOREIGN_KEY_CHECKS=0");
        DB::statement("ALTER TABLE `users` ADD `nickname` VARCHAR(40) NULL AFTER `name`");
        DB::update('UPDATE users SET nickname = name');
    }
};
"#;

    const ALTER: &str = r#"<?php
class AlterPosts extends Migration {
    public function up()
    {
        Schema::table('posts', function (Blueprint $t) {
            $t->renameColumn('title', 'headline');
            $t->string('slug')->after('headline')->unique();
            $t->dropColumn(['price']);
            $t->string('body', 2000)->nullable(false)->change();
            $t->dropForeign(['editor_id']);
            $t->dropIndex(['user_id', 'published']);
        });
        Schema::rename('password_reset_tokens', 'password_resets');
        Schema::dropIfExists('categories');
    }
}
"#;

    fn replay(files: &[&str]) -> Vec<(Schema, Renames, Vec<String>)> {
        let mut s = Schema::default();
        files
            .iter()
            .map(|f| {
                let mut r = Renames::default();
                let w = apply(&mut s, f, &mut r);
                (s.clone(), r, w)
            })
            .collect()
    }

    #[test]
    fn reads_the_default_users_migration() {
        let steps = replay(&[USERS]);
        let (s, _, w) = &steps[0];
        assert!(w.is_empty(), "{w:?}");
        let users = s.table("users").unwrap();
        let names: Vec<&str> = users.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "name", "email", "email_verified_at", "password", "remember_token", "created_at", "updated_at"]);
        let id = users.column("id").unwrap();
        assert!(id.primary_key && id.auto_increment && id.data_type == DataType::BigInt);
        assert_eq!(users.column("name").unwrap().data_type, DataType::Varchar(255));
        assert!(!users.column("name").unwrap().nullable);
        assert!(users.column("email_verified_at").unwrap().nullable);
        assert_eq!(users.indexes[0].name, "users_email_unique");
        assert!(s.table("password_reset_tokens").unwrap().column("email").unwrap().primary_key);
        // Nothing of down() is applied.
        assert_eq!(s.tables.len(), 2);
    }

    #[test]
    fn reads_keys_types_and_defaults() {
        let steps = replay(&[USERS, POSTS]);
        let (s, _, w) = &steps[1];
        assert_eq!(w, &vec!["raw SQL not applied: CREATE EXTENSION IF NOT EXISTS pg_trgm".to_string()]);
        assert!(s.table("users").unwrap().column("nickname").is_some());
        let posts = s.table("posts").unwrap();
        let user = posts.column("user_id").unwrap().references.clone().unwrap();
        assert_eq!((user.table.as_str(), user.column.as_str(), user.on_delete), ("users", "id", RefAction::Cascade));
        let cat = posts.column("category_id").unwrap();
        assert!(cat.nullable);
        assert_eq!(cat.references.as_ref().unwrap().table, "categories");
        assert_eq!(posts.column("title").unwrap().data_type, DataType::Varchar(120));
        assert_eq!(posts.column("published").unwrap().default, Some(DefaultValue::Bool(false)));
        assert_eq!(posts.column("price").unwrap().data_type, DataType::Decimal(10, 2));
        assert_eq!(posts.column("status").unwrap().data_type, DataType::Enum(vec!["draft".into(), "live".into()]));
        assert_eq!(posts.column("status").unwrap().default, Some(DefaultValue::String("draft".into())));
        assert_eq!(posts.column("editor_id").unwrap().references.as_ref().unwrap().on_delete, RefAction::SetNull);
        assert!(posts.column("taggable_type").is_some() && posts.column("taggable_id").is_some());
        assert!(posts.indexes.iter().any(|i| i.name == "posts_user_id_published_index"));
        assert!(posts.column("deleted_at").unwrap().nullable);
    }

    #[test]
    fn alters_renames_and_drops() {
        let steps = replay(&[USERS, POSTS, ALTER]);
        let (s, r, w) = &steps[2];
        assert!(w.iter().all(|w| !w.contains("not modeled")), "{w:?}");
        let posts = s.table("posts").unwrap();
        assert!(posts.column("title").is_none());
        assert_eq!(posts.column("headline").unwrap().data_type, DataType::Varchar(120));
        assert!(posts.column("price").is_none());
        let body = posts.column("body").unwrap();
        assert_eq!((body.data_type.clone(), body.nullable), (DataType::Varchar(2000), false));
        assert!(posts.column("editor_id").unwrap().references.is_none());
        assert!(!posts.indexes.iter().any(|i| i.name == "posts_user_id_published_index"));
        assert!(posts.indexes.iter().any(|i| i.name == "posts_slug_unique"));
        assert!(s.table("password_resets").is_some());

        let c = changes(&steps[1].0, s, r);
        let p = c.table("posts").unwrap();
        assert_eq!(p.mark, Mark::Altered);
        assert_eq!(p.column("headline"), Mark::Renamed);
        assert_eq!(p.column("slug"), Mark::Added);
        assert_eq!(p.column("price"), Mark::Dropped);
        assert_eq!(p.column("body"), Mark::Altered);
        assert_eq!(p.column("editor_id"), Mark::Altered);
        assert_eq!(c.table("password_resets").unwrap().mark, Mark::Renamed);
        assert!(c.table("users").is_none());
    }

    #[test]
    fn steps_come_in_file_name_order() {
        let dir = std::env::temp_dir().join(format!("sdb-laravel-{}", std::process::id()));
        let migrations = dir.join("database").join("migrations");
        std::fs::create_dir_all(&migrations).unwrap();
        std::fs::write(migrations.join("2024_02_01_000000_create_posts_table.php"), POSTS).unwrap();
        std::fs::write(migrations.join("0001_01_01_000000_create_users_table.php"), USERS.replace('\n', "\r\n")).unwrap();
        std::fs::write(migrations.join("README.md"), "not a migration").unwrap();
        assert_eq!(find_dir(&migrations.join("..")).unwrap(), migrations);
        let steps = read_dir(&migrations).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!((steps[0].date.as_deref(), steps[0].title.as_str()), (Some("0001-01-01"), "create users table"));
        assert_eq!(steps[1].schema.tables.len(), 3);
        // posts -> categories, a table no migration here creates.
        let errors: Vec<_> = crate::validate::validate(&steps[1].schema).into_iter().filter(|i| i.severity == crate::validate::Severity::Error).collect();
        assert!(errors.iter().all(|e| e.message.contains("categories")), "{:?}", errors.iter().map(|e| &e.message).collect::<Vec<_>>());
    }

    #[test]
    fn plurals() {
        for (s, p) in [("user", "users"), ("category", "categories"), ("day", "days"), ("status", "statuses"), ("person", "people"), ("blog_post", "blog_posts"), ("box", "boxes")] {
            assert_eq!(plural(s), p);
        }
        assert_eq!(snake("BlogPost"), "blog_post");
    }

    #[test]
    fn heredoc_statements_are_warned_about() {
        let src = "<?php\nclass X { public function up() {\n DB::statement(<<<SQL\n    CREATE VIEW v AS SELECT 1\n    SQL);\n Schema::create('a', fn (Blueprint $t) => $t->id());\n} }\n";
        let mut s = Schema::default();
        let w = apply(&mut s, src, &mut Renames::default());
        assert_eq!(w, vec!["raw SQL not applied: CREATE VIEW v AS SELECT 1".to_string()]);
        assert!(s.table("a").unwrap().column("id").is_some());
    }
}
