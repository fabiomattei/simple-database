//! Rails migrations (`db/migrate/*.rb`) read without Ruby: the schema
//! statements of each file's `up` (or else `change`) are replayed onto a
//! [`Schema`], one [`Step`] per file, in file name order (Rails's order). In
//! `reversible` blocks only `dir.up` counts. `execute` is applied where it is
//! plain DDL (`laravel::raw_sql`). What can't be modeled (other SQL, unknown
//! methods, values computed at run time) becomes a warning on that step.
//! Control flow is ignored: every statement counts.

use crate::laravel::{plural, raw_sql, short};
use crate::schema::{Column, DataType, DefaultValue, ForeignKey, Index, RefAction, Schema, Table, TableForeignKey, unqualified};
use crate::timeline::{Renames, Step, Warning};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// `db/migrate` in `start` or the nearest folder above it that has one.
pub fn find_dir(start: &Path) -> Option<PathBuf> {
    start.ancestors().map(|d| d.join("db").join("migrate")).find(|d| d.is_dir())
}

/// The migrations folder `path` names: itself when it holds `.rb` files (and
/// isn't a Rails app's root), else the `db/migrate` of it or a folder above it.
pub fn resolve_dir(path: &Path) -> Option<PathBuf> {
    if crate::timeline::has_files(path, "rb") && !path.join("Gemfile").is_file() { Some(path.to_path_buf()) } else { find_dir(path) }
}

/// What carries over from one migration to the next besides the schema.
#[derive(Debug, Default)]
struct State {
    /// Postgres enum types (`create_enum`) and their values.
    enums: HashMap<String, Vec<String>>,
    /// Check constraints already reported as not modeled, so removing them is quiet.
    unmodeled_checks: Vec<String>,
}

/// The migration files of `dir`, with those of a `db/post_migrate` next to
/// it (GitLab's and Mastodon's post-deployment migrations, which Rails runs
/// with the others), in version order.
pub fn files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = vec![dir.to_path_buf()];
    if dir.file_name().is_some_and(|n| n == "migrate")
        && let Some(post) = dir.parent().map(|p| p.join("post_migrate"))
        && post.is_dir()
    {
        dirs.push(post);
    }
    let mut files = Vec::new();
    for d in dirs {
        let entries = std::fs::read_dir(&d).with_context(|| format!("cannot read {}", d.display()))?;
        files.extend(entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("rb"))));
    }
    files.sort_by_key(|p| p.file_name().map(|n| n.to_os_string()));
    Ok(files)
}

/// Every migration file of `dir` ([`files`]), replayed one after the other.
pub fn read_dir(dir: &Path) -> Result<Vec<Step>> {
    let files = files(dir)?;
    let mut schema = Schema::default();
    let mut state = State::default();
    let mut steps = Vec::new();
    for file in files {
        let src = std::fs::read_to_string(&file).with_context(|| format!("cannot read {}", file.display()))?;
        let mut renames = Renames::default();
        let warnings = apply_with(&mut schema, &mut state, &src, &mut renames);
        let id = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let (date, title) = split_id(&id);
        steps.push(Step { file, id, date, title, schema: schema.clone(), renames, warnings });
    }
    Ok(steps)
}

/// `20240115093000_create_users` → (`2024-01-15`, `create users`). An
/// engine's copy (`…_create_blobs.active_storage`) drops the engine.
fn split_id(id: &str) -> (Option<String>, String) {
    let (version, rest) = id.split_once('_').unwrap_or(("", id));
    let numbered = !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit());
    let rest = if numbered { rest } else { id };
    let title = rest.split('.').next().unwrap_or(rest).replace('_', " ");
    let date = (numbered && version.len() == 14).then(|| format!("{}-{}-{}", &version[..4], &version[4..6], &version[6..8]));
    (date, title)
}

/// Replays the `up` (or `change`) of one migration file onto `schema`; returns its warnings.
pub fn apply(schema: &mut Schema, src: &str, renames: &mut Renames) -> Vec<Warning> {
    apply_with(schema, &mut State::default(), src, renames)
}

fn apply_with(schema: &mut Schema, state: &mut State, src: &str, renames: &mut Renames) -> Vec<Warning> {
    let (toks, lines) = tokenize(src);
    let hashed_names = migration_version(&toks).is_some_and(|v| v >= (7, 1));
    let mut ctx = Ctx { schema, state, renames, warnings: Vec::new(), line: None, conditional: false, hashed_names };
    match method_body(&toks, "up").or_else(|| method_body(&toks, "change")) {
        Some(r) => ctx.run(&toks[r.clone()], &lines[r]),
        None => ctx.warn("no up or change method found".into()),
    }
    ctx.warnings
}

// ---- tokens

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A name or constant (`create_table`, `ActiveRecord`, `index_exists?`).
    Ident(String),
    /// `:users`, without the `:`.
    Sym(String),
    /// `null:` of `null: false`, without the `:`.
    Key(String),
    Str(String),
    /// A string with `#{…}` in it: made at run time.
    Interp(String),
    Num(String),
    /// The end of a line, where it ends a statement (not inside `(`, `[` or a hash).
    Nl,
    P(&'static str),
}

const PUNCT: [&str; 13] = ["**", "=>", "->", "::", "==", "!=", "&&", "||", "<=", ">=", "&.", "..", "<<"];
const SINGLE: &str = "(){}[],;.=!+-*/%<>?&|^~:";
const NAMES: [&str; 24] = ["(", ")", "{", "}", "[", "]", ",", ";", ".", "=", "!", "+", "-", "*", "/", "%", "<", ">", "?", "&", "|", "^", "~", ":"];

fn is_ident_start(ch: char) -> bool {
    ch.is_alphabetic() || ch == '_'
}

fn is_ident(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// A heredoc waiting for its body: its token, label and whether `#{}` interpolates.
struct Heredoc {
    tok: usize,
    label: String,
    interpolates: bool,
}

/// The tokens of `src`, and the line (from 1) each one is on.
fn tokenize(src: &str) -> (Vec<Tok>, Vec<usize>) {
    let c: Vec<char> = src.chars().collect();
    let mut out: Vec<Tok> = Vec::new();
    let mut lines: Vec<usize> = Vec::new();
    let mut line = 1;
    // Open brackets: `true` for a block's `{`, where newlines still end statements.
    let mut stack: Vec<bool> = Vec::new();
    let mut heredocs: Vec<Heredoc> = Vec::new();
    let at = |i: usize, s: &str| s.chars().enumerate().all(|(k, ch)| c.get(i + k) == Some(&ch));
    let line_start = |i: usize| i == 0 || c[i - 1] == '\n';
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        let here = line;
        let mut emit = |t: Tok, out: &mut Vec<Tok>| {
            out.push(t);
            lines.push(here);
        };
        if ch == '\n' {
            i += 1;
            line += 1;
            if stack.last().is_none_or(|block| *block) && out.last().is_some_and(|t| *t != Tok::Nl) {
                emit(Tok::Nl, &mut out);
            }
            for h in std::mem::take(&mut heredocs) {
                let mut body = Vec::new();
                while i < c.len() {
                    let end = c[i..].iter().position(|x| *x == '\n').map_or(c.len(), |k| i + k);
                    let text: String = c[i..end].iter().collect();
                    i = (end + 1).min(c.len());
                    line += 1;
                    if text.trim() == h.label {
                        break;
                    }
                    body.push(text.trim_end_matches('\r').to_string());
                }
                let body = body.join("\n");
                out[h.tok] = if h.interpolates && body.contains("#{") { Tok::Interp(body) } else { Tok::Str(body) };
            }
        } else if ch == '\\' && c.get(i + 1) == Some(&'\n') {
            i += 2;
            line += 1;
        } else if ch.is_whitespace() {
            i += 1;
        } else if ch == '#' {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if line_start(i) && at(i, "=begin") {
            while i < c.len() && !(line_start(i) && at(i, "=end")) {
                if c[i] == '\n' {
                    line += 1;
                }
                i += 1;
            }
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if line_start(i) && at(i, "__END__") {
            break;
        } else if ch == '"' || ch == '\'' || ch == '`' {
            let (s, interp, next) = quoted(&c, i + 1, ch, ch, ch != '\'');
            line += c[i..next].iter().filter(|x| **x == '\n').count();
            i = next;
            if c.get(i) == Some(&':') && c.get(i + 1) != Some(&':') && c.get(i + 1).is_some_and(|n| n.is_whitespace()) {
                i += 1;
                emit(Tok::Key(s), &mut out);
            } else {
                emit(if interp { Tok::Interp(s) } else { Tok::Str(s) }, &mut out);
            }
        } else if ch == '%' && let Some((kind, open)) = percent_literal(&c, i) {
            let close = closer(open);
            let start = i + if kind.is_some() { 3 } else { 2 };
            let (s, interp, next) = quoted(&c, start, open, close, !matches!(kind, Some('q' | 'w' | 'i')));
            line += c[i..next].iter().filter(|x| **x == '\n').count();
            i = next;
            match kind {
                Some('w' | 'W' | 'i' | 'I') => {
                    emit(Tok::P("["), &mut out);
                    for (k, word) in s.split_whitespace().enumerate() {
                        if k > 0 {
                            emit(Tok::P(","), &mut out);
                        }
                        let symbol = matches!(kind, Some('i' | 'I'));
                        emit(if symbol { Tok::Sym(word.into()) } else { Tok::Str(word.into()) }, &mut out);
                    }
                    emit(Tok::P("]"), &mut out);
                }
                _ => emit(if interp { Tok::Interp(s) } else { Tok::Str(s) }, &mut out),
            }
        } else if at(i, "<<") && let Some((label, interpolates, next)) = heredoc_start(&c, i + 2) {
            emit(Tok::Str(String::new()), &mut out);
            heredocs.push(Heredoc { tok: out.len() - 1, label, interpolates });
            i = next;
        } else if at(i, "::") {
            emit(Tok::P("::"), &mut out);
            i += 2;
        } else if ch == ':' && c.get(i + 1) == Some(&'"') {
            let (s, _, next) = quoted(&c, i + 2, '"', '"', true);
            i = next;
            emit(Tok::Sym(s), &mut out);
        } else if ch == ':' && c.get(i + 1).is_some_and(|n| is_ident_start(*n)) && !(i > 0 && is_ident(c[i - 1])) {
            let start = i + 1;
            i = ident_end(&c, start);
            emit(Tok::Sym(c[start..i].iter().collect()), &mut out);
        } else if is_ident_start(ch) || ((ch == '@' || ch == '$') && c.get(i + 1).is_some_and(|n| is_ident_start(*n) || *n == '@')) {
            let start = i;
            while i < c.len() && (c[i] == '@' || c[i] == '$') {
                i += 1;
            }
            i = ident_end(&c, i);
            let word: String = c[start..i].iter().collect();
            if c.get(i) == Some(&':') && c.get(i + 1) != Some(&':') {
                i += 1;
                emit(Tok::Key(word), &mut out);
            } else {
                emit(Tok::Ident(word), &mut out);
            }
        } else if ch.is_ascii_digit() {
            let start = i;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '_' || (c[i] == '.' && c.get(i + 1).is_some_and(|d| d.is_ascii_digit()))) {
                i += 1;
            }
            emit(Tok::Num(c[start..i].iter().filter(|d| **d != '_').collect()), &mut out);
        } else if let Some(p) = PUNCT.iter().find(|p| at(i, p)) {
            emit(Tok::P(p), &mut out);
            i += p.len();
        } else if let Some(k) = SINGLE.find(ch) {
            match ch {
                '(' | '[' => stack.push(false),
                '{' => {
                    let block = matches!(out.last(), Some(Tok::Ident(_) | Tok::P(")" | "->")));
                    stack.push(block);
                }
                ')' | ']' | '}' => {
                    stack.pop();
                }
                _ => {}
            }
            emit(Tok::P(NAMES[k]), &mut out);
            i += 1;
        } else {
            i += 1;
        }
    }
    (out, lines)
}

/// The end of a name starting at `i`, with a trailing `?` or `!` (not `!=`).
fn ident_end(c: &[char], mut i: usize) -> usize {
    while i < c.len() && is_ident(c[i]) {
        i += 1;
    }
    if i < c.len() && (c[i] == '?' || c[i] == '!') && c.get(i + 1) != Some(&'=') {
        i += 1;
    }
    i
}

fn closer(open: char) -> char {
    match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        x => x,
    }
}

/// `%w[`, `%i(`, `%q{`, `%Q|`, `%{`: the letter (if any) and the opening delimiter.
fn percent_literal(c: &[char], i: usize) -> Option<(Option<char>, char)> {
    let delim = |ch: char| "([{<|!/".contains(ch);
    match (c.get(i + 1).copied(), c.get(i + 2).copied()) {
        (Some(k @ ('w' | 'W' | 'i' | 'I' | 'q' | 'Q')), Some(d)) if delim(d) => Some((Some(k), d)),
        (Some(d), _) if delim(d) && d != '/' => Some((None, d)),
        _ => None,
    }
}

/// The text of a literal starting at `i` (just after its opening `open`) up to
/// the matching `close`: the text, whether it interpolates `#{}`, and where it ends.
fn quoted(c: &[char], mut i: usize, open: char, close: char, escapes: bool) -> (String, bool, usize) {
    let mut s = String::new();
    let mut interp = false;
    let mut depth = 0;
    while i < c.len() {
        let ch = c[i];
        if ch == '\\' && i + 1 < c.len() {
            let e = c[i + 1];
            match e {
                'n' if escapes => s.push('\n'),
                't' if escapes => s.push('\t'),
                _ if e == close || e == '\\' || e == open || (escapes && (e == '"' || e == '#')) => s.push(e),
                _ => {
                    s.push('\\');
                    s.push(e);
                }
            }
            i += 2;
            continue;
        }
        if escapes && ch == '#' && c.get(i + 1) == Some(&'{') {
            interp = true;
            let mut d = 0;
            while i < c.len() {
                s.push(c[i]);
                match c[i] {
                    '{' => d += 1,
                    '}' => {
                        d -= 1;
                        if d == 0 {
                            i += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            continue;
        }
        if ch == close && depth == 0 {
            return (s, interp, i + 1);
        }
        if open != close {
            if ch == open {
                depth += 1;
            } else if ch == close {
                depth -= 1;
            }
        }
        s.push(ch);
        i += 1;
    }
    (s, interp, i)
}

/// `<<~SQL`, `<<-SQL`, `<<SQL`, `<<~'SQL'` with `i` just after `<<`: the
/// label, whether the body interpolates, and where the opener ends.
fn heredoc_start(c: &[char], mut i: usize) -> Option<(String, bool, usize)> {
    if matches!(c.get(i), Some('~' | '-')) {
        i += 1;
    }
    let quote = match c.get(i) {
        Some(q @ ('\'' | '"')) => {
            i += 1;
            Some(*q)
        }
        _ => None,
    };
    let start = i;
    while i < c.len() && is_ident(c[i]) {
        i += 1;
    }
    let label: String = c[start..i].iter().collect();
    // A bare `<<name` is a heredoc only in capitals; `a << b` is an append.
    if label.is_empty() || (quote.is_none() && !label.chars().next().is_some_and(|ch| ch.is_ascii_uppercase())) {
        return None;
    }
    if let Some(q) = quote {
        if c.get(i) != Some(&q) {
            return None;
        }
        i += 1;
    }
    Some((label, quote != Some('\''), i))
}

/// Whether the token at `k` starts a statement.
fn stmt_start(t: &[Tok], k: usize) -> bool {
    k == 0
        || match &t[k - 1] {
            Tok::Nl | Tok::P(";" | "{" | "|" | "(" | "=") => true,
            Tok::Ident(w) => matches!(w.as_str(), "do" | "then" | "else" | "begin" | "ensure"),
            _ => false,
        }
}

/// The `end` closing a block whose body starts at `from`.
fn block_end(t: &[Tok], from: usize) -> Option<usize> {
    let mut depth = 1;
    for k in from..t.len() {
        let Tok::Ident(w) = &t[k] else { continue };
        if k > 0 && matches!(t[k - 1], Tok::P("." | "::" | "&.")) {
            continue;
        }
        match w.as_str() {
            "def" | "class" | "module" | "begin" | "case" | "do" => depth += 1,
            "if" | "unless" | "while" | "until" if stmt_start(t, k) => depth += 1,
            "end" => {
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

/// `ActiveRecord::Migration[7.1]` → (7, 1).
fn migration_version(t: &[Tok]) -> Option<(u32, u32)> {
    let k = t.windows(4).position(|w| matches!(&w[0], Tok::Ident(n) if n == "Migration" || n == "Schema") && w[1] == Tok::P("[") && matches!(w[2], Tok::Num(_)) && w[3] == Tok::P("]"))?;
    let Tok::Num(v) = &t[k + 2] else { return None };
    let (major, minor) = v.split_once('.').unwrap_or((v, "0"));
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Where the body of `def name` (or `def self.name`) is.
fn method_body(t: &[Tok], name: &str) -> Option<std::ops::Range<usize>> {
    let def = (0..t.len()).find(|&k| {
        t[k] == Tok::Ident("def".into()) && {
            let n = if t.get(k + 1) == Some(&Tok::Ident("self".into())) && t.get(k + 2) == Some(&Tok::P(".")) { k + 3 } else { k + 1 };
            t.get(n) == Some(&Tok::Ident(name.into()))
        }
    })?;
    let start = def + t[def..].iter().position(|x| *x == Tok::Nl || *x == Tok::P(";"))? + 1;
    Some(start..block_end(t, start)?)
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
    Sym(String),
    Num(String),
    Bool(bool),
    Null,
    Array(Vec<Val>),
    Hash(Vec<(String, Val)>),
    /// `-> { "CURRENT_TIMESTAMP" }`: SQL.
    Raw(String),
    /// A string with `#{…}`: made at run time.
    Interp(String),
    /// Anything else, as source-ish text.
    Other(String),
}

impl Val {
    /// A string or symbol.
    fn name(&self) -> Option<&str> {
        match self {
            Val::Str(s) | Val::Sym(s) => Some(s),
            _ => None,
        }
    }

    /// A column list: one name or an array of them.
    fn names(&self) -> Vec<String> {
        match self {
            Val::Array(items) => items.iter().filter_map(|v| v.name().map(str::to_string)).collect(),
            v => v.name().map(|n| vec![n.to_string()]).unwrap_or_default(),
        }
    }

    fn truthy(&self) -> Option<bool> {
        match self {
            Val::Bool(b) => Some(*b),
            Val::Null => Some(false),
            _ => None,
        }
    }

    fn get(&self, key: &str) -> Option<&Val> {
        match self {
            Val::Hash(h) => h.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn text(&self) -> String {
        match self {
            Val::Str(s) => format!("{s:?}"),
            Val::Sym(s) => format!(":{s}"),
            Val::Num(n) => n.clone(),
            Val::Bool(b) => b.to_string(),
            Val::Null => "nil".into(),
            Val::Array(items) => format!("[{}]", items.iter().map(Val::text).collect::<Vec<_>>().join(", ")),
            Val::Hash(h) => format!("{{ {} }}", h.iter().map(|(k, v)| format!("{k}: {}", v.text())).collect::<Vec<_>>().join(", ")),
            Val::Raw(s) => format!("-> {{ {s:?} }}"),
            Val::Interp(s) => format!("{s:?}"),
            Val::Other(s) => s.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct Arg {
    key: Option<String>,
    val: Val,
}

/// A `do |var| … end` or `{ |var| … }` block.
#[derive(Debug, Clone)]
struct Block {
    var: Option<String>,
    body: Vec<Tok>,
    lines: Vec<usize>,
}

#[derive(Debug, Clone)]
struct Call {
    name: String,
    args: Vec<Arg>,
    block: Option<Block>,
}

impl Call {
    fn positional(&self) -> impl Iterator<Item = &Val> {
        self.args.iter().filter(|a| a.key.is_none()).map(|a| &a.val)
    }

    fn pos(&self, k: usize) -> Option<&Val> {
        self.positional().nth(k)
    }

    fn pos_name(&self, k: usize) -> Option<String> {
        self.pos(k).and_then(Val::name).map(str::to_string)
    }

    /// Every option: `key: value` arguments, then those of a `{ … }` argument.
    fn options(&self) -> impl Iterator<Item = (&str, &Val)> {
        let named = self.args.iter().filter_map(|a| a.key.as_deref().map(|k| (k, &a.val)));
        let hashed = self.positional().filter_map(|v| match v {
            Val::Hash(h) => Some(h.iter().map(|(k, v)| (k.as_str(), v))),
            _ => None,
        });
        named.chain(hashed.flatten())
    }

    fn opt(&self, key: &str) -> Option<&Val> {
        self.options().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    fn flag(&self, key: &str) -> Option<bool> {
        self.opt(key).and_then(Val::truthy)
    }

    fn str_opt(&self, key: &str) -> Option<String> {
        self.opt(key).and_then(Val::name).map(str::to_string)
    }

    fn num_opt(&self, key: &str) -> Option<u32> {
        match self.opt(key)? {
            Val::Num(n) => n.parse().ok(),
            _ => None,
        }
    }

    /// The call without its first positional argument (a table name), and that argument.
    fn shift(&self, name: &str) -> (Option<Val>, Call) {
        let mut rest = self.clone();
        rest.name = name.to_string();
        let first = rest.args.iter().position(|a| a.key.is_none()).map(|k| rest.args.remove(k).val);
        (first, rest)
    }
}

struct Parser<'a> {
    t: &'a [Tok],
    lines: &'a [usize],
    i: usize,
}

const STOP_WORDS: [&str; 6] = ["do", "if", "unless", "end", "and", "or"];

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

    fn stop_word(&self) -> bool {
        self.ident(0).is_some_and(|w| STOP_WORDS.contains(&w))
    }

    /// At the end of an expression: `,` `)` `]` `}` `;`, a line end or a
    /// `do` / `if` / `unless` after it.
    fn at_end(&self) -> bool {
        matches!(self.peek(0), None | Some(Tok::Nl | Tok::P("," | ")" | "]" | "}" | ";" | "=>"))) || self.stop_word()
    }

    /// The call at the cursor (a name), with its arguments and block.
    fn call(&mut self) -> Call {
        let name = self.ident(0).unwrap_or_default().to_string();
        self.i += 1;
        let args = if self.is(0, "(") {
            self.paren_args()
        } else if self.bare_start() {
            self.bare_args()
        } else {
            Vec::new()
        };
        let block = self.block();
        Call { name, args, block }
    }

    /// Whether arguments without parentheses follow.
    fn bare_start(&self) -> bool {
        match self.peek(0) {
            Some(Tok::Str(_) | Tok::Interp(_) | Tok::Sym(_) | Tok::Key(_) | Tok::Num(_)) => true,
            Some(Tok::Ident(_)) => !self.stop_word(),
            Some(Tok::P(p)) => matches!(*p, "[" | "->" | "-"),
            _ => false,
        }
    }

    fn arg(&mut self) -> Arg {
        let key = match (self.peek(0), self.is(1, "=>")) {
            (Some(Tok::Key(k)), _) => {
                let k = k.clone();
                self.i += 1;
                Some(k)
            }
            (Some(Tok::Sym(k) | Tok::Str(k)), true) => {
                let k = k.clone();
                self.i += 2;
                Some(k)
            }
            _ => None,
        };
        Arg { key, val: self.value() }
    }

    fn paren_args(&mut self) -> Vec<Arg> {
        let close = matching(self.t, self.i).unwrap_or(self.t.len());
        self.i += 1;
        let mut out = Vec::new();
        while self.i < close {
            out.push(self.arg());
            if self.is(0, ",") {
                self.i += 1;
            } else if self.i < close {
                self.skip_expr();
                if self.i < close && !self.is(0, ",") {
                    self.i += 1;
                }
            }
        }
        self.i = close + 1;
        out
    }

    fn bare_args(&mut self) -> Vec<Arg> {
        let mut out = Vec::new();
        loop {
            out.push(self.arg());
            if !self.is(0, ",") {
                break;
            }
            self.i += 1;
            while self.peek(0) == Some(&Tok::Nl) {
                self.i += 1;
            }
        }
        out
    }

    /// One expression; the cursor ends at the end of it.
    fn value(&mut self) -> Val {
        let start = self.i;
        if let Some(v) = self.simple_value()
            && self.at_end()
        {
            return v;
        }
        self.i = start;
        self.skip_expr();
        Val::Other(text(&self.t[start..self.i]))
    }

    fn simple_value(&mut self) -> Option<Val> {
        let v = match self.peek(0)? {
            Tok::Str(s) | Tok::Interp(s) => {
                let v = if matches!(self.peek(0), Some(Tok::Str(_))) { Val::Str(s.clone()) } else { Val::Interp(s.clone()) };
                self.i += 1;
                // `<<~SQL.squish` and the like leave SQL as SQL.
                while self.is(0, ".") && self.ident(1).is_some_and(|m| matches!(m, "squish" | "strip" | "freeze" | "chomp" | "strip_heredoc" | "dup")) {
                    self.i += 2;
                }
                v
            }
            Tok::Sym(s) => {
                let s = s.clone();
                self.i += 1;
                Val::Sym(s)
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
                let close = matching(self.t, self.i)?;
                self.i += 1;
                let mut items = Vec::new();
                while self.i < close {
                    items.push(self.value());
                    if self.is(0, ",") {
                        self.i += 1;
                    } else if self.i < close {
                        self.i = close;
                    }
                }
                self.i = close + 1;
                Val::Array(items)
            }
            Tok::P("{") => {
                let close = matching(self.t, self.i)?;
                self.i += 1;
                let mut entries = Vec::new();
                while self.i < close {
                    let a = self.arg();
                    if let Some(k) = a.key {
                        entries.push((k, a.val));
                    }
                    if self.is(0, ",") {
                        self.i += 1;
                    } else if self.i < close {
                        self.i = close;
                    }
                }
                self.i = close + 1;
                Val::Hash(entries)
            }
            Tok::P("->") => self.lambda(1)?,
            Tok::Ident(id) => match id.as_str() {
                "true" | "false" => {
                    let b = id == "true";
                    self.i += 1;
                    Val::Bool(b)
                }
                "nil" => {
                    self.i += 1;
                    Val::Null
                }
                "lambda" | "proc" => self.lambda(1)?,
                _ => return None,
            },
            _ => return None,
        };
        Some(v)
    }

    /// `-> { "SQL" }` (the cursor on `->`, its body `skip` tokens on): the
    /// string as SQL, anything else as text.
    fn lambda(&mut self, skip: usize) -> Option<Val> {
        let start = self.i;
        self.i += skip;
        if self.is(0, "(") {
            self.i = matching(self.t, self.i)? + 1;
        }
        if !self.is(0, "{") {
            return None;
        }
        let close = matching(self.t, self.i)?;
        let inner = &self.t[self.i + 1..close];
        self.i = close + 1;
        Some(match inner {
            [Tok::Str(s)] => Val::Raw(s.clone()),
            _ => Val::Other(text(&self.t[start..self.i])),
        })
    }

    /// Moves to the end of the current expression.
    fn skip_expr(&mut self) {
        let mut depth = 0i32;
        while let Some(x) = self.peek(0) {
            match x {
                Tok::P("(" | "[" | "{") => depth += 1,
                Tok::P(")" | "]" | "}") if depth == 0 => return,
                Tok::P(")" | "]" | "}") => depth -= 1,
                Tok::P("," | ";") | Tok::Nl if depth == 0 => return,
                Tok::Ident(w) if depth == 0 && STOP_WORDS.contains(&w.as_str()) => return,
                _ => {}
            }
            self.i += 1;
        }
    }

    /// A `do |v| … end` or `{ |v| … }` block at the cursor, if there is one.
    fn block(&mut self) -> Option<Block> {
        let (start, end, next) = if self.ident(0) == Some("do") {
            let start = self.i + 1;
            let end = block_end(self.t, start)?;
            (start, end, end + 1)
        } else if self.is(0, "{") {
            let end = matching(self.t, self.i)?;
            (self.i + 1, end, end + 1)
        } else {
            return None;
        };
        let mut body = start;
        let mut var = None;
        if self.t.get(body) == Some(&Tok::P("|")) {
            let close = body + 1 + self.t[body + 1..end].iter().position(|x| *x == Tok::P("|"))?;
            var = self.t[body + 1..close].iter().find_map(|x| match x {
                Tok::Ident(v) => Some(v.clone()),
                _ => None,
            });
            body = close + 1;
        }
        self.i = next;
        Some(Block { var, body: self.t[body..end].to_vec(), lines: self.lines[body..end].to_vec() })
    }
}

/// Tokens as rough Ruby text, for warnings.
fn text(t: &[Tok]) -> String {
    let mut s = String::new();
    for x in t {
        let piece = match x {
            Tok::Ident(i) => i.clone(),
            Tok::Sym(v) => format!(":{v}"),
            Tok::Key(k) => format!("{k}:"),
            Tok::Str(v) | Tok::Interp(v) => format!("{v:?}"),
            Tok::Num(n) => n.clone(),
            Tok::Nl => continue,
            Tok::P(p) => p.to_string(),
        };
        let glue = matches!(x, Tok::P("." | "::" | "(" | ")" | "," | "]" | "&.")) || s.ends_with(['(', '.', '[', ':']) || s.is_empty();
        if !glue {
            s.push(' ');
        }
        s.push_str(&piece);
    }
    s
}

// ---- replaying

struct Ctx<'a> {
    schema: &'a mut Schema,
    state: &'a mut State,
    renames: &'a mut Renames,
    warnings: Vec<Warning>,
    /// The line of the statement being replayed, for its warnings.
    line: Option<usize>,
    /// The statement runs only under a condition (`if index_exists?(…)`, an
    /// `if` block, after `return unless …`), so what it acts on may well be missing.
    conditional: bool,
    /// Long index names are shortened with a hash (`Migration[7.1]` and later).
    hashed_names: bool,
}

/// Migration methods that act on one table, and the `t.` method of a
/// `change_table` block that does the same.
const TABLE_METHODS: [(&str, &str); 27] = [
    ("add_column", "column"),
    ("remove_column", "remove_one"),
    ("remove_columns", "remove"),
    ("rename_column", "rename"),
    ("change_column", "change"),
    ("change_column_default", "change_default"),
    ("change_column_null", "change_null"),
    ("change_column_comment", "change_comment"),
    ("add_index", "index"),
    ("remove_index", "remove_index"),
    ("rename_index", "rename_index"),
    ("add_reference", "references"),
    ("add_belongs_to", "references"),
    ("remove_reference", "remove_references"),
    ("remove_belongs_to", "remove_references"),
    ("add_foreign_key", "foreign_key"),
    ("remove_foreign_key", "remove_foreign_key"),
    ("add_timestamps", "timestamps"),
    ("remove_timestamps", "remove_timestamps"),
    ("add_check_constraint", "check_constraint"),
    ("remove_check_constraint", "remove_check_constraint"),
    ("add_unique_constraint", "unique_constraint"),
    ("remove_unique_constraint", "remove_unique_constraint"),
    ("change_table_comment", "table_comment"),
    ("add_exclusion_constraint", "exclusion_constraint"),
    ("add_attachment", "attachment"),
    ("remove_attachment", "remove_attachment"),
];

/// Migration methods that change nothing sdb models.
const IGNORED_METHODS: [&str; 9] =
    ["enable_extension", "disable_extension", "create_schema", "drop_schema", "add_exclusion_constraint", "remove_exclusion_constraint", "create_virtual_table", "drop_virtual_table", "rename_schema"];

/// `t.<type>` methods.
const TYPES: [&str; 41] = [
    "string", "text", "integer", "bigint", "smallint", "float", "decimal", "numeric", "datetime", "timestamp", "timestamptz", "time", "date", "binary", "blob",
    "boolean", "bool", "json", "jsonb", "uuid", "citext", "inet", "cidr", "macaddr", "hstore", "xml", "money", "interval", "point", "line", "ltree", "tsvector", "bit",
    "bit_varying", "daterange", "tsrange", "tstzrange", "int4range", "int8range", "numrange", "enum",
];

/// Column options that change nothing sdb models (or are read where they belong).
const IGNORED_OPTIONS: [&str; 23] = [
    "limit", "precision", "scale", "type", "collation", "charset", "after", "first", "if_not_exists", "if_exists", "enum_type", "values", "using", "unsigned",
    "srid", "geographic", "polymorphic", "foreign_key", "to_table", "name", "force", "cast_as", "auto_generated",
];

impl Ctx<'_> {
    fn warn(&mut self, message: String) {
        let w = Warning { line: self.line, message };
        if !self.warnings.contains(&w) {
            self.warnings.push(w);
        }
    }

    /// A warning that what a statement acts on doesn't exist, unless the
    /// statement is guarded by a condition.
    fn missing(&mut self, message: String) {
        if !self.conditional {
            self.warn(message);
        }
    }

    fn exists(&self, table: &str) -> bool {
        self.schema.table(table).is_some()
    }

    fn table_mut(&mut self, name: &str) -> &mut Table {
        self.schema.tables.iter_mut().find(|t| t.name == name).expect("table exists")
    }

    fn column_mut(&mut self, table: &str, column: &str) -> Option<&mut Column> {
        self.schema.tables.iter_mut().find(|t| t.name == table)?.columns.iter_mut().find(|c| c.name == column)
    }

    /// Every migration statement of a body, in order.
    fn run(&mut self, t: &[Tok], lines: &[usize]) {
        let in_if = conditional_tokens(t);
        let outer = self.conditional;
        // After `return if …` / `return unless …` the rest runs only sometimes.
        let mut guarded = false;
        let mut p = Parser { t, lines, i: 0 };
        while p.i < t.len() {
            if !stmt_start(t, p.i) {
                p.i += 1;
                continue;
            }
            let Some(name) = p.ident(0).map(str::to_string) else {
                p.i += 1;
                continue;
            };
            self.line = lines.get(p.i).copied();
            // A `rescue` branch handles errors: not part of the migration.
            if name == "rescue" {
                p.i = block_end(t, p.i + 1).unwrap_or(t.len());
                continue;
            }
            if name == "return" {
                guarded |= t[p.i..].iter().take_while(|x| **x != Tok::Nl).any(|x| matches!(x, Tok::Ident(w) if w == "if" || w == "unless"));
                p.i += 1;
                continue;
            }
            self.conditional = outer || guarded || in_if[p.i] || modified(t, p.i);
            if p.is(1, ".") && name == "connection" && p.ident(2).is_some() {
                p.i += 2;
                continue;
            }
            // `dir.down { … }` in a `reversible` block is the way back.
            if p.is(1, ".") && p.ident(2) == Some("down") {
                p.i += 3;
                let _ = p.block();
                continue;
            }
            if p.is(1, ".") || p.is(1, "::") || p.is(1, "=") {
                p.i += 1;
                continue;
            }
            let known = TABLE_METHODS.iter().any(|(m, _)| *m == name)
                || IGNORED_METHODS.contains(&name.as_str())
                || matches!(
                    name.as_str(),
                    "create_table"
                        | "change_table"
                        | "drop_table"
                        | "rename_table"
                        | "create_join_table"
                        | "drop_join_table"
                        | "execute"
                        | "create_enum"
                        | "drop_enum"
                        | "rename_enum"
                        | "add_enum_value"
                        | "rename_enum_value"
                        | "revert"
                );
            let schema_like = ["add_", "remove_", "change_", "create_", "drop_", "rename_"].iter().any(|s| name.starts_with(s));
            if known || schema_like {
                let c = p.call();
                self.statement(&c);
            } else {
                // Another method on a table (a project's own helper): its effect is unknown.
                let first = if p.is(1, "(") { p.peek(2) } else { p.peek(1) };
                if let Some(Tok::Sym(table) | Tok::Str(table)) = first
                    && self.exists(table)
                    && !name.ends_with('?')
                {
                    let table = table.clone();
                    self.warn(format!("{name} on {table} not modeled"));
                }
                p.i += 1;
            }
        }
        self.conditional = outer;
    }

    fn statement(&mut self, c: &Call) {
        let n = c.name.as_str();
        if let Some((_, m)) = TABLE_METHODS.iter().find(|(top, _)| *top == n) {
            let (table, rest) = c.shift(m);
            let Some(mut table) = table.as_ref().and_then(Val::name).map(str::to_string) else {
                return self.warn(format!("{n} with a table name computed at run time"));
            };
            if !self.exists(&table) {
                return self.missing(format!("{n} on {table}, which doesn't exist"));
            }
            return self.table_cmd(&mut table, &rest, false);
        }
        match n {
            "create_table" => self.create_table(c),
            "change_table" => {
                let Some(table) = c.pos_name(0) else { return self.warn("change_table with a table name computed at run time".into()) };
                if !self.exists(&table) {
                    return self.missing(format!("change_table on {table}, which doesn't exist"));
                }
                if let Some(b) = &c.block {
                    self.table_block(table, b);
                }
            }
            "drop_table" | "drop_join_table" => {
                let names: Vec<String> = if n == "drop_join_table" {
                    match (c.pos_name(0), c.pos_name(1)) {
                        (Some(a), Some(b)) => vec![c.str_opt("table_name").unwrap_or_else(|| join_table_name(&a, &b))],
                        _ => vec![],
                    }
                } else {
                    c.positional().filter_map(Val::name).map(str::to_string).collect()
                };
                if names.is_empty() {
                    return self.warn(format!("{n} with a table name computed at run time"));
                }
                for name in names {
                    if !self.exists(&name) && c.flag("if_exists") != Some(true) {
                        self.missing(format!("{n} on {name}, which doesn't exist"));
                    }
                    self.schema.drop_table(&name);
                }
            }
            "rename_table" => match (c.pos_name(0), c.pos_name(1)) {
                (Some(from), Some(to)) => self.rename_table(&from, &to),
                _ => self.warn("rename_table with a name computed at run time".into()),
            },
            "create_join_table" => self.create_join_table(c),
            "execute" => match c.pos(0) {
                Some(Val::Str(sql)) => {
                    for w in raw_sql::apply(self.schema, sql, self.renames) {
                        self.warn(w);
                    }
                }
                // Rows changed with values made at run time change no schema.
                Some(Val::Interp(sql)) if starts_with_row_statement(sql) => {}
                other => self.warn(format!("SQL built at run time not applied: {}", short(&other.map(Val::text).unwrap_or_default()))),
            },
            "create_enum" => match (c.pos_name(0), c.pos(1)) {
                (Some(name), Some(values)) => {
                    self.state.enums.insert(name, values.names());
                }
                _ => self.warn("create_enum with values computed at run time".into()),
            },
            "drop_enum" => {
                if let Some(name) = c.pos_name(0) {
                    self.state.enums.remove(&name);
                }
            }
            "rename_enum" => {
                if let (Some(from), Some(to)) = (c.pos_name(0), c.pos_name(1).or_else(|| c.str_opt("to")))
                    && let Some(values) = self.state.enums.remove(&from)
                {
                    self.state.enums.insert(to, values);
                }
            }
            "add_enum_value" | "rename_enum_value" => {
                let Some(name) = c.pos_name(0) else { return };
                let Some(before) = self.state.enums.get(&name).cloned() else { return self.warn(format!("{n} on enum {name}, which isn't known")) };
                let mut after = before.clone();
                if n == "add_enum_value" {
                    let Some(value) = c.pos_name(1) else { return };
                    let at = |key: &str| c.str_opt(key).and_then(|v| after.iter().position(|x| *x == v));
                    match (at("before"), at("after")) {
                        (Some(k), _) => after.insert(k, value),
                        (_, Some(k)) => after.insert(k + 1, value),
                        _ => after.push(value),
                    }
                } else if let (Some(from), Some(to)) = (c.str_opt("from"), c.str_opt("to")) {
                    for v in after.iter_mut().filter(|v| **v == from) {
                        *v = to.clone();
                    }
                }
                // Columns of the type are the ones with its values.
                for col in self.schema.tables.iter_mut().flat_map(|t| t.columns.iter_mut()) {
                    if col.data_type == DataType::Enum(before.clone()) {
                        col.data_type = DataType::Enum(after.clone());
                    }
                }
                self.state.enums.insert(name, after);
            }
            "revert" => self.warn("revert is not modeled".into()),
            n if IGNORED_METHODS.contains(&n) => {}
            n => self.warn(format!("{n} not modeled")),
        }
    }

    fn create_table(&mut self, c: &Call) {
        let Some(name) = c.pos_name(0) else { return self.warn("create_table with a table name computed at run time".into()) };
        if self.exists(&name) {
            if c.flag("if_not_exists") == Some(true) {
                return;
            }
            if c.opt("force").is_none() {
                self.warn(format!("{name} is created again; the earlier one is replaced"));
            }
            self.schema.drop_table(&name);
        }
        if c.opt("as").is_some() {
            self.warn(format!("{name}: create_table as a query is not modeled"));
        }
        let mut tb = Table::new(name.clone());
        tb.comment = c.str_opt("comment").filter(|s| !s.is_empty());
        match (c.opt("id"), c.opt("primary_key")) {
            (_, Some(Val::Array(cols))) => tb.composite_pk = Val::Array(cols.clone()).names(),
            (Some(Val::Bool(false)), _) => {}
            (id, pk) => {
                let key = pk.and_then(Val::name).unwrap_or("id");
                let kind = id.and_then(Val::name).unwrap_or("primary_key");
                let mut col = match kind {
                    "primary_key" | "bigint" | "bigserial" => auto_key(key, DataType::BigInt),
                    "integer" | "serial" => auto_key(key, DataType::Int),
                    "uuid" => {
                        let mut col = Column::new(key, DataType::Uuid);
                        col.default = Some(DefaultValue::Expr("gen_random_uuid()".into()));
                        col
                    }
                    other => Column::new(key, self.rails_type(other, c, key).unwrap_or(DataType::Custom(other.into()))),
                };
                col.primary_key = true;
                col.nullable = false;
                if let Some(v) = c.opt("default") {
                    match default_value(v, &col.data_type) {
                        Ok(d) => col.default = d,
                        Err(text) => self.warn(format!("{name}.{key}: default {} not modeled", short(&text))),
                    }
                }
                tb.columns.push(col);
            }
        }
        self.schema.tables.push(tb);
        if let Some(b) = &c.block {
            self.table_block(name, b);
        }
    }

    fn create_join_table(&mut self, c: &Call) {
        let (Some(a), Some(b)) = (c.pos_name(0), c.pos_name(1)) else { return self.warn("create_join_table with names computed at run time".into()) };
        let name = c.str_opt("table_name").unwrap_or_else(|| join_table_name(&a, &b));
        if self.exists(&name) {
            self.warn(format!("{name} is created again; the earlier one is replaced"));
            self.schema.drop_table(&name);
        }
        let mut tb = Table::new(name.clone());
        let nullable = c.opt("column_options").and_then(|o| o.get("null")).and_then(Val::truthy).unwrap_or(false);
        for t in [&a, &b] {
            let mut col = Column::new(format!("{}_id", singular(t)), DataType::BigInt);
            col.nullable = nullable;
            tb.columns.push(col);
        }
        self.schema.tables.push(tb);
        if let Some(block) = &c.block {
            self.table_block(name, block);
        }
    }

    fn rename_table(&mut self, from: &str, to: &str) {
        if !self.exists(from) {
            return self.missing(format!("rename_table of {from}, which doesn't exist"));
        }
        // Rails renames the indexes it named after the table.
        let hashed = self.hashed_names;
        let named: Vec<bool> = self.table_mut(from).indexes.iter().map(|i| i.name == index_name(from, &i.columns, hashed)).collect();
        self.schema.rename_table(from, to);
        self.renames.table(from, to);
        for (i, named) in self.table_mut(to).indexes.iter_mut().zip(named) {
            if named {
                i.name = index_name(to, &i.columns, hashed);
            }
        }
    }

    /// The `t.` commands of a `create_table` / `change_table` block.
    fn table_block(&mut self, mut table: String, b: &Block) {
        let Some(var) = b.var.clone() else { return };
        let mut p = Parser { t: &b.body, lines: &b.lines, i: 0 };
        while p.i < b.body.len() {
            let after_dot = p.i > 0 && matches!(b.body[p.i - 1], Tok::P("." | "&."));
            if !after_dot && p.ident(0) == Some(&var) && p.is(1, ".") && p.ident(2).is_some() {
                self.line = b.lines.get(p.i).copied();
                let outer = self.conditional;
                self.conditional |= modified(&b.body, p.i);
                p.i += 2;
                let c = p.call();
                self.table_cmd(&mut table, &c, true);
                self.conditional = outer;
            } else {
                p.i += 1;
            }
        }
    }

    /// One `t.<name>` command on `table` (or a migration method turned into one).
    fn table_cmd(&mut self, table: &mut String, c: &Call, in_block: bool) {
        let t = table.clone();
        let m = c.name.as_str();
        let label = if in_block { format!("t.{m}") } else { m.to_string() };
        match m {
            "column" => match (c.pos_name(0), c.pos(1)) {
                (Some(name), Some(ty)) => self.add_column(&t, &name, ty, c),
                _ => self.warn(format!("{t}: {label} with a name or type computed at run time")),
            },
            "primary_key" => match c.pos_name(0) {
                Some(name) => {
                    let ty = c.pos(1).cloned().unwrap_or(Val::Sym("primary_key".into()));
                    self.add_column(&t, &name, &ty, c);
                    if let Some(col) = self.column_mut(&t, &name) {
                        col.primary_key = true;
                        col.nullable = false;
                        col.auto_increment = matches!(col.data_type, DataType::Int | DataType::BigInt) && matches!(ty, Val::Sym(ref s) if s == "primary_key");
                    }
                }
                None => self.warn(format!("{t}: {label} with a name computed at run time")),
            },
            "references" | "belongs_to" => {
                for name in c.positional().filter_map(Val::name).map(str::to_string).collect::<Vec<_>>() {
                    self.reference(&t, &name, c);
                }
            }
            "timestamps" => {
                for name in ["created_at", "updated_at"] {
                    let mut col = Column::new(name, DataType::Timestamp);
                    col.nullable = c.flag("null").unwrap_or(false);
                    if let Some(v) = c.opt("default") {
                        self.set_default(&t, &mut col, v);
                    }
                    self.push_column(&t, col);
                }
            }
            "index" => match c.pos(0) {
                Some(cols) => {
                    let cols = cols.clone();
                    self.index(&t, &cols, c);
                }
                None => self.warn(format!("{t}: {label} with columns computed at run time")),
            },
            "remove" | "remove_one" => {
                let names: Vec<String> = if m == "remove_one" { c.pos_name(0).into_iter().collect() } else { c.positional().flat_map(Val::names).collect() };
                for col in names {
                    if self.column_mut(&t, &col).is_none() && c.flag("if_exists") != Some(true) {
                        self.missing(format!("{t}: {label} {col}, which doesn't exist"));
                    }
                    self.schema.drop_column(&t, &col);
                }
            }
            "rename" => match (c.pos_name(0), c.pos_name(1)) {
                (Some(from), Some(to)) => self.rename_column(&t, &from, &to, &label),
                _ => self.warn(format!("{t}: {label} with a name computed at run time")),
            },
            "change" => match (c.pos_name(0), c.pos(1)) {
                (Some(name), Some(ty)) => {
                    let Some(old) = self.column_mut(&t, &name).cloned() else { return self.warn(format!("{t}: {label} of {name}, which doesn't exist")) };
                    let Some(data_type) = self.type_of(&t, &name, ty, c) else { return };
                    let mut col = Column { data_type, ..old };
                    let extra = self.column_options(&t, &mut col, c);
                    *self.column_mut(&t, &name).expect("column exists") = col;
                    for i in extra {
                        self.add_index(&t, i);
                    }
                }
                _ => self.warn(format!("{t}: {label} with a name or type computed at run time")),
            },
            "change_default" => {
                let (Some(name), Some(v)) = (c.pos_name(0), new_value(c, 1)) else { return };
                let Some(mut col) = self.column_mut(&t, &name).cloned() else { return self.warn(format!("{t}: {label} of {name}, which doesn't exist")) };
                self.set_default(&t, &mut col, &v);
                *self.column_mut(&t, &name).expect("column exists") = col;
            }
            "change_null" => {
                let (Some(name), Some(v)) = (c.pos_name(0), c.pos(1).and_then(Val::truthy)) else { return };
                match self.column_mut(&t, &name) {
                    Some(col) => col.nullable = v && !col.primary_key,
                    None => self.warn(format!("{t}: {label} of {name}, which doesn't exist")),
                }
            }
            "change_comment" => {
                let (Some(name), Some(v)) = (c.pos_name(0), new_value(c, 1)) else { return };
                let text = comment(&v);
                match self.column_mut(&t, &name) {
                    Some(col) => col.comment = text,
                    None => self.warn(format!("{t}: {label} of {name}, which doesn't exist")),
                }
            }
            "table_comment" => {
                if let Some(v) = new_value(c, 0) {
                    self.table_mut(&t).comment = comment(&v);
                }
            }
            "remove_index" => {
                let name = c.str_opt("name");
                let cols = c.opt("column").or_else(|| c.pos(0)).map(Val::names).unwrap_or_default();
                let tb = self.table_mut(&t);
                let before = tb.indexes.len();
                match &name {
                    Some(n) => tb.indexes.retain(|i| i.name != *n),
                    None => tb.indexes.retain(|i| i.columns != cols),
                }
                if tb.indexes.len() == before && c.flag("if_exists") != Some(true) {
                    let what = name.unwrap_or_else(|| cols.join(", "));
                    self.missing(format!("{t}: {label} {what}, which doesn't exist"));
                }
            }
            "rename_index" => {
                if let (Some(from), Some(to)) = (c.pos_name(0), c.pos_name(1)) {
                    match self.table_mut(&t).indexes.iter_mut().find(|i| i.name == from) {
                        Some(i) => i.name = to,
                        None => self.missing(format!("{t}: {label} {from}, which doesn't exist")),
                    }
                }
            }
            "remove_references" | "remove_belongs_to" => {
                for name in c.positional().filter_map(Val::name).map(str::to_string).collect::<Vec<_>>() {
                    self.schema.drop_column(&t, &format!("{name}_id"));
                    if c.flag("polymorphic") == Some(true) {
                        self.schema.drop_column(&t, &format!("{name}_type"));
                    }
                }
            }
            // Paperclip's attachment columns.
            "attachment" | "remove_attachment" => {
                for name in c.positional().filter_map(Val::name).map(str::to_string).collect::<Vec<_>>() {
                    let columns = [("file_name", DataType::Varchar(255)), ("content_type", DataType::Varchar(255)), ("file_size", DataType::Int), ("updated_at", DataType::Timestamp)];
                    for (suffix, data_type) in columns {
                        let column = format!("{name}_{suffix}");
                        if m == "attachment" {
                            self.push_column(&t, Column::new(column, data_type));
                        } else {
                            self.schema.drop_column(&t, &column);
                        }
                    }
                }
            }
            "remove_timestamps" => {
                self.schema.drop_column(&t, "created_at");
                self.schema.drop_column(&t, "updated_at");
            }
            "foreign_key" => self.foreign_key(&t, c, &label),
            "remove_foreign_key" => self.remove_foreign_key(&t, c, &label),
            "check_constraint" => {
                let Some(expr) = c.pos_name(0) else { return self.warn(format!("{t}: {label} computed at run time")) };
                let name = c.str_opt("name");
                let column = name.as_deref().and_then(|n| self.schema.table(&t)?.columns.iter().find(|col| crate::ddl::check_name(&t, &col.name) == n)).map(|col| col.name.clone());
                match column {
                    Some(col) => self.column_mut(&t, &col).expect("column exists").check = Some(expr),
                    None => {
                        self.warn(format!("{t}: check constraint {} not modeled (sdb keeps one check per column, named <table>_<column>_check)", name.clone().unwrap_or(expr)));
                        self.state.unmodeled_checks.extend(name);
                    }
                }
            }
            "remove_check_constraint" => {
                let name = c.str_opt("name");
                let expr = c.pos_name(0);
                let unmodeled = name.as_ref().is_some_and(|n| self.state.unmodeled_checks.contains(n));
                let tb = self.table_mut(&t);
                let tname = tb.name.clone();
                let hit = tb.columns.iter_mut().find(|col| {
                    col.check.is_some() && (name.as_deref() == Some(crate::ddl::check_name(&tname, &col.name).as_str()) || (expr.is_some() && col.check == expr))
                });
                match hit {
                    Some(col) => col.check = None,
                    None if unmodeled => {}
                    None => self.missing(format!("{t}: {label} of a check it doesn't know")),
                }
            }
            "unique_constraint" => {
                let cols = c.pos(0).map(Val::names).unwrap_or_default();
                match cols.as_slice() {
                    [] => self.warn(format!("{t}: {label} with columns computed at run time")),
                    [one] => match self.column_mut(&t, one) {
                        Some(col) => col.unique = true,
                        None => self.warn(format!("{t}: {label} on {one}, which doesn't exist")),
                    },
                    _ => {
                        let name = c.str_opt("name").unwrap_or_else(|| Index::default_name(&t, &cols, true));
                        self.add_index(&t, Index::new(name, cols, true));
                    }
                }
            }
            "remove_unique_constraint" => {
                let cols = c.pos(0).map(Val::names).unwrap_or_default();
                let name = c.str_opt("name");
                let tb = self.table_mut(&t);
                let tname = tb.name.clone();
                if let Some(col) = tb.columns.iter_mut().find(|col| col.unique && (cols == [col.name.clone()] || name.as_deref() == Some(crate::ddl::unique_name(&tname, &col.name).as_str()))) {
                    col.unique = false;
                } else {
                    tb.indexes.retain(|i| !(i.unique && (Some(&i.name) == name.as_ref() || (!cols.is_empty() && i.columns == cols))));
                }
            }
            "exclusion_constraint" | "remove_exclusion_constraint" => self.warn(format!("{t}: {label} not modeled")),
            "rename_table" => {
                if let Some(to) = c.pos_name(0) {
                    self.rename_table(&t, &to);
                    *table = to;
                }
            }
            ty if TYPES.contains(&ty) || ty == "virtual" => {
                let names: Vec<String> = c.positional().filter_map(Val::name).map(str::to_string).collect();
                if names.is_empty() {
                    return self.warn(format!("{t}: {label} with a name computed at run time"));
                }
                for name in names {
                    self.add_column(&t, &name, &Val::Sym(ty.into()), c);
                }
            }
            other => self.warn(format!("{t}: {}{other} not modeled", if in_block { "t." } else { "" })),
        }
    }

    /// The type a `t.column` / `add_column` type argument names; warns when none.
    fn type_of(&mut self, t: &str, column: &str, ty: &Val, c: &Call) -> Option<DataType> {
        match ty {
            Val::Sym(s) => match self.rails_type(s, c, column) {
                Some(d) => Some(d),
                None => {
                    self.warn(format!("{t}.{column}: {s} column not modeled"));
                    None
                }
            },
            Val::Str(s) => Some(DataType::parse(s)),
            other => {
                self.warn(format!("{t}.{column}: type {} computed at run time", short(&other.text())));
                None
            }
        }
    }

    /// A Rails type (with its `limit`, `precision`, … options) as sdb's;
    /// `None` for a generated (`virtual`) column.
    fn rails_type(&self, ty: &str, c: &Call, column: &str) -> Option<DataType> {
        let limit = c.num_opt("limit");
        Some(match ty {
            "string" => DataType::Varchar(limit.unwrap_or(255)),
            "text" | "mediumtext" | "longtext" | "tinytext" => DataType::Text,
            "integer" => match limit {
                Some(1 | 2) => DataType::SmallInt,
                Some(5..=8) => DataType::BigInt,
                _ => DataType::Int,
            },
            "int" | "int4" | "serial" => DataType::Int,
            "bigint" | "int8" | "bigserial" | "primary_key" => DataType::BigInt,
            "smallint" | "int2" => DataType::SmallInt,
            "float" | "real" | "float8" | "double" => DataType::Real,
            "bool" => DataType::Bool,
            "varchar" | "character_varying" => DataType::Varchar(limit.unwrap_or(255)),
            "decimal" | "numeric" => DataType::Decimal(c.num_opt("precision").unwrap_or(10), c.num_opt("scale").unwrap_or(0)),
            "datetime" | "timestamp" | "timestamptz" => DataType::Timestamp,
            "time" => DataType::Time,
            "date" => DataType::Date,
            "binary" | "blob" => DataType::Blob,
            "boolean" => DataType::Bool,
            "json" | "jsonb" => DataType::Json,
            "uuid" => DataType::Uuid,
            "enum" => {
                if let Some(values) = c.opt("values") {
                    DataType::Enum(values.names())
                } else {
                    let name = c.str_opt("enum_type").unwrap_or_else(|| column.to_string());
                    match self.state.enums.get(&name) {
                        Some(values) => DataType::Enum(values.clone()),
                        None => DataType::Custom(name),
                    }
                }
            }
            "virtual" => return None,
            other => DataType::Custom(other.to_string()),
        })
    }

    fn add_column(&mut self, t: &str, name: &str, ty: &Val, c: &Call) {
        let Some(data_type) = self.type_of(t, name, ty, c) else { return };
        if let DataType::Custom(n) = &data_type
            && matches!(ty, Val::Sym(s) if s == "enum")
        {
            self.warn(format!("{t}.{name}: enum type {n} isn't known"));
        }
        let mut col = Column::new(name, data_type);
        if matches!(ty, Val::Sym(s) if s == "primary_key") {
            col.primary_key = true;
            col.auto_increment = true;
            col.nullable = false;
        }
        let extra = self.column_options(t, &mut col, c);
        self.push_column(t, col);
        for i in extra {
            self.add_index(t, i);
        }
    }

    /// Applies a column's options; returns the indexes they ask for.
    fn column_options(&mut self, t: &str, col: &mut Column, c: &Call) -> Vec<Index> {
        let mut extra = Vec::new();
        let options: Vec<(String, Val)> = c.options().map(|(k, v)| (k.to_string(), v.clone())).collect();
        for (key, v) in &options {
            match key.as_str() {
                "null" => col.nullable = v.truthy().unwrap_or(true) && !col.primary_key,
                // After the loop, once `array:` has set the type.
                "default" => {}
                "comment" => col.comment = comment(v),
                "primary_key" if v.truthy() == Some(true) => {
                    col.primary_key = true;
                    col.nullable = false;
                }
                "auto_increment" if v.truthy() == Some(true) => col.auto_increment = true,
                "index" => {
                    if let Some(i) = index_option(t, std::slice::from_ref(&col.name), v, self.hashed_names) {
                        extra.push(i);
                    }
                }
                "unique" => col.unique = v.truthy() == Some(true),
                "array" if v.truthy() == Some(true) => {
                    let element = crate::ddl::type_sql(&col.data_type, crate::ddl::Dialect::Postgres).to_ascii_lowercase();
                    col.data_type = DataType::Custom(format!("{element}[]"));
                }
                "primary_key" | "auto_increment" | "array" => {}
                k if IGNORED_OPTIONS.contains(&k) => {}
                k => self.warn(format!("{t}.{}: option {k} not modeled", col.name)),
            }
        }
        if let Some(v) = c.opt("default") {
            self.set_default(t, col, v);
        }
        extra
    }

    fn set_default(&mut self, t: &str, col: &mut Column, v: &Val) {
        match default_value(v, &col.data_type) {
            Ok(d) => col.default = d,
            Err(text) => self.warn(format!("{t}.{}: default {} not modeled", col.name, short(&text))),
        }
    }

    /// `t.references :user`: `user_id` (and `user_type` when polymorphic),
    /// indexed unless `index: false`, with a foreign key when asked.
    fn reference(&mut self, t: &str, name: &str, c: &Call) {
        let polymorphic = c.opt("polymorphic").is_some_and(|v| v.truthy() != Some(false));
        let id = format!("{name}_id");
        let ty = c.opt("type").cloned().unwrap_or(Val::Sym("bigint".into()));
        let Some(data_type) = self.type_of(t, &id, &ty, c) else { return };
        let mut col = Column::new(&id, data_type);
        col.nullable = c.flag("null").unwrap_or(true);
        if let Some(v) = c.opt("default") {
            self.set_default(t, &mut col, v);
        }
        col.comment = c.opt("comment").and_then(comment);
        match c.opt("foreign_key") {
            None | Some(Val::Bool(false) | Val::Null) => {}
            Some(_) if polymorphic => self.warn(format!("{t}.{id}: a polymorphic reference can't have a foreign key")),
            Some(fk) => {
                let table = fk.get("to_table").and_then(Val::name).map(str::to_string).unwrap_or_else(|| plural(name));
                let column = fk.get("primary_key").and_then(Val::name).unwrap_or("id").to_string();
                let on_delete = fk.get("on_delete").map_or(RefAction::NoAction, ref_action);
                let on_update = fk.get("on_update").map_or(RefAction::NoAction, ref_action);
                col.references = Some(ForeignKey { table, column, on_delete, on_update });
            }
        }
        let mut parts = vec![id];
        let nullable = col.nullable;
        self.push_column(t, col);
        if polymorphic {
            let mut kind = Column::new(format!("{name}_type"), DataType::Varchar(255));
            kind.nullable = nullable;
            self.push_column(t, kind);
            parts.insert(0, format!("{name}_type"));
        }
        let index = match c.opt("index") {
            None => index_option(t, &parts, &Val::Bool(true), self.hashed_names),
            Some(v) => index_option(t, &parts, v, self.hashed_names),
        };
        if let Some(i) = index {
            self.add_index(t, i);
        }
    }

    fn index(&mut self, t: &str, cols: &Val, c: &Call) {
        let parts: Vec<String> = match cols {
            Val::Str(s) if s.contains(['(', ' ', ',']) => vec![Index::expression_part(s)],
            v => v.names(),
        };
        if parts.is_empty() {
            return self.warn(format!("{t}: index with columns computed at run time"));
        }
        let name = c.str_opt("name").unwrap_or_else(|| index_name(t, &parts, self.hashed_names));
        let mut i = Index::new(name, parts, c.flag("unique") == Some(true));
        i.predicate = c.str_opt("where").map(|p| unwrapped(&p).to_string());
        i.method = c.str_opt("using").map(|m| m.to_ascii_lowercase()).filter(|m| m != "btree");
        self.add_index(t, i);
    }

    fn push_column(&mut self, t: &str, col: Column) {
        let tb = self.table_mut(t);
        match tb.columns.iter_mut().find(|x| x.name == col.name) {
            Some(old) => {
                let name = col.name.clone();
                *old = col;
                self.warn(format!("{t}.{name} is added again; the earlier one is replaced"));
            }
            None => tb.columns.push(col),
        }
    }

    /// Adds an index; a unique one over one column named as sdb names a
    /// column's UNIQUE (`users_email_key`, or on MySQL the column's name)
    /// becomes that column's `unique`.
    fn add_index(&mut self, t: &str, i: Index) {
        if let [col] = i.columns.as_slice()
            && i.unique
            && i.predicate.is_none()
            && i.method.is_none()
            && (i.name == crate::ddl::unique_name(t, col) || i.name == *col)
            && let Some(c) = self.column_mut(t, col)
        {
            c.unique = true;
            return;
        }
        let tb = self.table_mut(t);
        tb.indexes.retain(|x| x.name != i.name);
        tb.indexes.push(i);
    }

    fn rename_column(&mut self, t: &str, from: &str, to: &str, label: &str) {
        if self.column_mut(t, from).is_none() {
            return self.missing(format!("{t}: {label} {from}, which doesn't exist"));
        }
        // Rails renames the indexes it named after the column.
        let hashed = self.hashed_names;
        let named: Vec<bool> = self.table_mut(t).indexes.iter().map(|i| i.name == index_name(t, &i.columns, hashed)).collect();
        self.schema.rename_column(t, from, to);
        self.renames.column(t, from, to);
        let tb = self.table_mut(t);
        for (i, named) in tb.indexes.iter_mut().zip(named) {
            if named {
                i.name = index_name(t, &i.columns, hashed);
            }
        }
    }

    fn foreign_key(&mut self, t: &str, c: &Call, label: &str) {
        let Some(to) = c.pos_name(0) else { return self.warn(format!("{t}: {label} to a table computed at run time")) };
        let columns = c.opt("column").map(Val::names).unwrap_or_else(|| vec![format!("{}_id", singular(&to))]);
        let ref_columns = c.opt("primary_key").map(Val::names).unwrap_or_else(|| vec!["id".into()]);
        let on_delete = c.opt("on_delete").map_or(RefAction::NoAction, ref_action);
        let on_update = c.opt("on_update").map_or(RefAction::NoAction, ref_action);
        let tb = self.table_mut(t);
        if let [col] = columns.as_slice() {
            match tb.columns.iter_mut().find(|x| x.name == *col) {
                Some(x) => x.references = Some(ForeignKey { table: to, column: ref_columns.into_iter().next().unwrap_or_else(|| "id".into()), on_delete, on_update }),
                None => self.warn(format!("{t}: {label} on {col}, which doesn't exist")),
            }
            return;
        }
        tb.foreign_keys.retain(|fk| fk.columns != columns);
        tb.foreign_keys.push(TableForeignKey { columns, table: to, ref_columns, on_delete, on_update });
    }

    fn remove_foreign_key(&mut self, t: &str, c: &Call, label: &str) {
        let to = c.pos_name(0).or_else(|| c.str_opt("to_table"));
        let columns = c.opt("column").map(Val::names);
        let name = c.str_opt("name");
        let tb = self.table_mut(t);
        let tname = tb.name.clone();
        let matches = |cols: &[String], table: &str| {
            if let Some(n) = &name {
                let joined = cols.join("_");
                return *n == crate::ddl::fk_name(&tname, &joined) || *n == foreign_key_name(&tname, &joined);
            }
            match &columns {
                Some(want) => want == cols,
                None => to.as_deref() == Some(table),
            }
        };
        let mut hit = false;
        for col in &mut tb.columns {
            if let Some(fk) = &col.references
                && matches(std::slice::from_ref(&col.name), &fk.table)
            {
                col.references = None;
                hit = true;
            }
        }
        let before = tb.foreign_keys.len();
        tb.foreign_keys.retain(|fk| !matches(&fk.columns, &fk.table));
        hit |= tb.foreign_keys.len() != before;
        if !hit && c.flag("if_exists") != Some(true) {
            let what = name.or(to).unwrap_or_default();
            self.missing(format!("{t}: {label} {what}, which it can't find"));
        }
    }
}

/// Whether the token at `k` starts a statement with an `if` / `unless`
/// modifier at its end (`remove_index :t, :c if index_exists?(:t, :c)`).
fn modified(t: &[Tok], k: usize) -> bool {
    let mut depth = 0i32;
    for x in &t[k..] {
        match x {
            Tok::P("(" | "[" | "{") => depth += 1,
            Tok::P(")" | "]" | "}") => depth -= 1,
            Tok::Nl | Tok::P(";") if depth <= 0 => return false,
            Tok::Ident(w) if depth == 0 && (w == "if" || w == "unless") => return true,
            Tok::Ident(w) if depth == 0 && w == "do" => return false,
            _ => {}
        }
    }
    false
}

/// For each token, whether it is inside an `if` / `unless` / `case` block.
fn conditional_tokens(t: &[Tok]) -> Vec<bool> {
    let mut out = vec![false; t.len()];
    // Open blocks: whether each is conditional.
    let mut stack: Vec<bool> = Vec::new();
    for k in 0..t.len() {
        out[k] = stack.contains(&true);
        let Tok::Ident(w) = &t[k] else { continue };
        if k > 0 && matches!(t[k - 1], Tok::P("." | "::" | "&.")) {
            continue;
        }
        match w.as_str() {
            "def" | "class" | "module" | "begin" | "do" => stack.push(false),
            "case" => stack.push(true),
            "if" | "unless" | "while" | "until" if stmt_start(t, k) => stack.push(true),
            "end" => {
                stack.pop();
            }
            _ => {}
        }
    }
    out
}

/// Whether SQL changes rows or the session rather than the schema.
fn starts_with_row_statement(sql: &str) -> bool {
    let word = sql.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    matches!(word.as_str(), "update" | "insert" | "delete" | "select" | "with" | "set" | "truncate" | "analyze" | "refresh" | "reindex" | "vacuum")
}

/// The name Rails gives a foreign key: `fk_rails_` and the start of the
/// SHA-256 of `<table>_<column>_fk`.
pub fn foreign_key_name(table: &str, column: &str) -> String {
    format!("fk_rails_{}", &sha256_hex(&format!("{table}_{column}_fk"))[..10])
}

/// The value a `change_column_default` / `…_comment` sets: argument `k`, or
/// the `to:` of `from: …, to: …`.
fn new_value(c: &Call, k: usize) -> Option<Val> {
    match c.pos(k) {
        Some(v) => Some(v.get("to").unwrap_or(v).clone()),
        None => c.opt("to").cloned(),
    }
}

fn auto_key(name: &str, t: DataType) -> Column {
    let mut c = Column::new(name, t);
    c.primary_key = true;
    c.auto_increment = true;
    c.nullable = false;
    c
}

/// A comment option: text, `nil`, or `{ from:, to: }`.
fn comment(v: &Val) -> Option<String> {
    let v = v.get("to").unwrap_or(v);
    v.name().filter(|s| !s.is_empty()).map(str::to_string)
}

/// An `index:` option (`true`, or `{ unique: true, name: … }`) as the index it asks for.
fn index_option(t: &str, cols: &[String], v: &Val, hashed: bool) -> Option<Index> {
    match v {
        Val::Bool(true) => Some(Index::new(index_name(t, cols, hashed), cols.to_vec(), false)),
        Val::Hash(_) => {
            let name = v.get("name").and_then(Val::name).map_or_else(|| index_name(t, cols, hashed), str::to_string);
            let mut i = Index::new(name, cols.to_vec(), v.get("unique").and_then(Val::truthy) == Some(true));
            i.predicate = v.get("where").and_then(Val::name).map(|p| unwrapped(p).to_string());
            Some(i)
        }
        _ => None,
    }
}

/// `on_delete: :cascade` and the like.
fn ref_action(v: &Val) -> RefAction {
    match v.name() {
        Some("cascade") => RefAction::Cascade,
        Some("nullify") => RefAction::SetNull,
        Some("restrict") => RefAction::Restrict,
        _ => RefAction::NoAction,
    }
}

/// A `default:` value as sdb's, or the source text when it can't be one.
fn default_value(v: &Val, t: &DataType) -> Result<Option<DefaultValue>, String> {
    Ok(Some(match v {
        Val::Null => return Ok(None),
        Val::Str(s) if is_numeric(t) && s.parse::<f64>().is_ok() => DefaultValue::Number(s.clone()),
        Val::Str(s) | Val::Sym(s) => DefaultValue::String(s.clone()),
        Val::Num(n) if *t == DataType::Bool && (n == "0" || n == "1") => DefaultValue::Bool(n == "1"),
        Val::Num(n) => DefaultValue::Number(n.clone()),
        Val::Bool(b) if *t == DataType::Bool => DefaultValue::Bool(*b),
        Val::Bool(b) => DefaultValue::Number(if *b { "1" } else { "0" }.into()),
        // A Postgres array: `'{a,b}'`.
        Val::Array(items) if matches!(t, DataType::Custom(n) if n.ends_with("[]")) => {
            let parts: Option<Vec<String>> = items.iter().map(|v| v.name().map(str::to_string).or_else(|| if let Val::Num(n) = v { Some(n.clone()) } else { None })).collect();
            let parts = parts.ok_or_else(|| v.text())?;
            DefaultValue::Expr(format!("'{{{}}}'", parts.join(",").replace('\'', "''")))
        }
        Val::Raw(e) => {
            let lower = e.trim().to_ascii_lowercase();
            // MySQL takes a literal default of `text` and the like only in parentheses: `('')`.
            let inner = e.trim().strip_prefix('(').and_then(|x| x.strip_suffix(')')).map(str::trim);
            if lower.starts_with("current_timestamp") || lower == "now()" {
                DefaultValue::Now
            } else if let Some(text) = inner.and_then(|x| x.strip_prefix('\'')?.strip_suffix('\'')).filter(|x| !x.replace("''", "").contains('\'')) {
                DefaultValue::String(text.replace("''", "'"))
            } else if let Some(n) = inner.filter(|x| x.parse::<f64>().is_ok()) {
                DefaultValue::Number(n.to_string())
            } else {
                DefaultValue::Expr(e.clone())
            }
        }
        other => return Err(other.text()),
    }))
}

fn is_numeric(t: &DataType) -> bool {
    matches!(t, DataType::Int | DataType::BigInt | DataType::SmallInt | DataType::Real | DataType::Decimal(..))
}

/// `((a = 1))` → `a = 1`.
fn unwrapped(mut s: &str) -> &str {
    while let Some(inner) = Index::expression(s.trim()) {
        s = inner;
    }
    s.trim()
}

/// Rails's index name: `index_posts_on_user_id_and_created_at`. Since Rails
/// 7.1 (`hashed`) one over 62 bytes is `idx_on_<columns>_<hash>`.
fn index_name(table: &str, cols: &[String], hashed: bool) -> String {
    let parts: Vec<String> = cols
        .iter()
        .map(|c| match Index::expression(c) {
            Some(e) => e.split(|ch: char| !(ch.is_alphanumeric() || ch == '_')).filter(|w| !w.is_empty()).collect::<Vec<_>>().join("_"),
            None => c.clone(),
        })
        .collect();
    let name = format!("index_{}_on_{}", unqualified(table), parts.join("_and_"));
    if !hashed || name.len() <= 62 {
        return name;
    }
    let mut short = format!("idx_on_{}", parts.join("_"));
    let mut limit = 62 - 11;
    while !short.is_char_boundary(limit.min(short.len())) {
        limit -= 1;
    }
    short.truncate(limit);
    format!("{short}_{}", &sha256_hex(&name)[..10])
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The table `create_join_table :products, :categories` makes:
/// `categories_products`, a shared prefix written once (`music_artists`,
/// `music_records` → `music_artists_records`).
fn join_table_name(a: &str, b: &str) -> String {
    let (a, b) = if a <= b { (a, b) } else { (b, a) };
    for (k, _) in a.match_indices('_').collect::<Vec<_>>().into_iter().rev() {
        let prefix = &a[..=k];
        if let Some(rest) = b.strip_prefix(prefix)
            && !rest.is_empty()
            && k + 1 < a.len()
        {
            return format!("{prefix}{}_{rest}", &a[k + 1..]);
        }
    }
    format!("{a}_{b}")
}

/// English singular of the last word of a snake_case name, the inverse of
/// [`plural`] for common words: `categories` → `category`.
pub fn singular(name: &str) -> String {
    let (head, word) = match name.rfind('_') {
        Some(i) => (&name[..=i], &name[i + 1..]),
        None => ("", name),
    };
    const IRREGULAR: [(&str, &str); 10] = [
        ("people", "person"),
        ("children", "child"),
        ("men", "man"),
        ("women", "woman"),
        ("mice", "mouse"),
        ("geese", "goose"),
        ("teeth", "tooth"),
        ("feet", "foot"),
        ("oxen", "ox"),
        ("leaves", "leaf"),
    ];
    const SAME: [&str; 12] = ["data", "equipment", "information", "rice", "money", "species", "series", "fish", "sheep", "deer", "news", "media"];
    let lower = word.to_ascii_lowercase();
    let s = if let Some((_, s)) = IRREGULAR.iter().find(|(p, _)| *p == lower) {
        s.to_string()
    } else if SAME.contains(&lower.as_str()) || lower.ends_with("ss") || lower.ends_with("us") || lower.len() < 2 {
        word.to_string()
    } else if let Some(stem) = lower.strip_suffix("ies") {
        format!("{}y", &word[..stem.len()])
    } else if ["sses", "shes", "ches", "xes", "zes", "uses"].iter().any(|e| lower.ends_with(e)) {
        word[..word.len() - 2].to_string()
    } else if lower.ends_with('s') {
        word[..word.len() - 1].to_string()
    } else {
        word.to_string()
    };
    format!("{head}{s}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::Dialect;
    use crate::framework::{self, Framework};
    use crate::timeline::{Mark, changes};

    fn replay(files: &[&str]) -> (Schema, Vec<Vec<Warning>>) {
        let mut schema = Schema::default();
        let mut state = State::default();
        let warnings = files.iter().map(|src| apply_with(&mut schema, &mut state, src, &mut Renames::default())).collect();
        (schema, warnings)
    }

    const USERS: &str = r#"# frozen_string_literal: true

class CreateUsers < ActiveRecord::Migration[8.0]
  def change
    create_table :users do |t|
      t.string :name, null: false
      t.string :email, null: false, limit: 120
      t.boolean :admin, default: false, null: false
      t.decimal :balance, precision: 10, scale: 2, default: "0.0"
      t.datetime :confirmed_at, default: -> { "CURRENT_TIMESTAMP" }
      t.timestamps
    end
    add_index :users, :email, unique: true
  end
end
"#;

    const POSTS: &str = r#"class CreatePosts < ActiveRecord::Migration[7.1]
  def change
    create_table :posts, comment: "Blog posts" do |t|
      t.references :user, null: false, foreign_key: { on_delete: :cascade }
      t.references :category, foreign_key: true, index: false
      t.references :commentable, polymorphic: true
      t.string :title, :slug, null: false
      t.text :body # the text
      t.integer :views, limit: 8, default: 0
      t.json :meta
      t.index [:user_id, :created_at]
      t.timestamps null: true
    end

    create_table :categories, id: :integer do |t|
      t.string :name
    end
  end
end
"#;

    #[test]
    fn create_table_columns_and_indexes() {
        let (s, w) = replay(&[USERS]);
        assert_eq!(w, vec![vec![]]);
        let users = s.table("users").unwrap();
        let names: Vec<&str> = users.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "name", "email", "admin", "balance", "confirmed_at", "created_at", "updated_at"]);
        let id = users.column("id").unwrap();
        assert!(id.primary_key && id.auto_increment && id.data_type == DataType::BigInt);
        assert_eq!(users.column("email").unwrap().data_type, DataType::Varchar(120));
        assert!(!users.column("email").unwrap().nullable);
        assert_eq!(users.column("admin").unwrap().default, Some(DefaultValue::Bool(false)));
        assert_eq!(users.column("balance").unwrap().data_type, DataType::Decimal(10, 2));
        assert_eq!(users.column("confirmed_at").unwrap().default, Some(DefaultValue::Now));
        assert!(!users.column("created_at").unwrap().nullable);
        assert_eq!(users.indexes, vec![Index::new("index_users_on_email", vec!["email".into()], true)]);
    }

    #[test]
    fn references_and_foreign_keys() {
        let (s, w) = replay(&[USERS, POSTS]);
        assert_eq!(w[1], vec![]);
        let posts = s.table("posts").unwrap();
        assert_eq!(posts.comment.as_deref(), Some("Blog posts"));
        let user = posts.column("user_id").unwrap();
        assert_eq!(user.data_type, DataType::BigInt);
        assert!(!user.nullable);
        assert_eq!(user.references, Some(ForeignKey { table: "users".into(), column: "id".into(), on_delete: RefAction::Cascade, on_update: RefAction::NoAction }));
        assert_eq!(posts.column("category_id").unwrap().references.as_ref().unwrap().table, "categories");
        assert!(posts.column("commentable_type").is_some());
        assert_eq!(posts.column("views").unwrap().data_type, DataType::BigInt);
        assert!(posts.column("slug").is_some_and(|c| !c.nullable));
        assert!(posts.column("created_at").unwrap().nullable);
        let index_names: Vec<&str> = posts.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(index_names, ["index_posts_on_user_id", "index_posts_on_commentable_type_and_commentable_id", "index_posts_on_user_id_and_created_at"]);
        assert_eq!(s.table("categories").unwrap().column("id").unwrap().data_type, DataType::Int);
    }

    #[test]
    fn alterations_renames_and_drops() {
        let alter = r#"class Changes < ActiveRecord::Migration[8.0]
  def up
    add_column :users, :nickname, :string, limit: 40
    add_reference :users, :manager, foreign_key: { to_table: :users }
    rename_column :users, :email, :email_address
    change_column :users, :name, :text
    change_column_null :users, :name, true
    change_column_default :users, :admin, from: false, to: true
    remove_column :users, :balance, :decimal
    rename_table :users, :people
    remove_index :people, :manager_id
    drop_table :posts, if_exists: true
  end

  def down
    raise ActiveRecord::IrreversibleMigration
  end
end
"#;
        let (s, w) = replay(&[USERS, alter]);
        assert_eq!(w[1], vec![]);
        assert!(s.table("users").is_none());
        let people = s.table("people").unwrap();
        assert_eq!(people.column("nickname").unwrap().data_type, DataType::Varchar(40));
        assert_eq!(people.column("manager_id").unwrap().references.as_ref().unwrap().table, "people");
        assert!(people.column("email").is_none() && people.column("email_address").is_some());
        let name = people.column("name").unwrap();
        assert!(name.data_type == DataType::Text && name.nullable);
        assert_eq!(people.column("admin").unwrap().default, Some(DefaultValue::Bool(true)));
        assert!(people.column("balance").is_none());
        // Indexes Rails named after the table and column follow the renames.
        let index_names: Vec<&str> = people.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(index_names, ["index_people_on_email_address"]);

        let mut renames = Renames::default();
        let mut before = Schema::default();
        apply(&mut before, USERS, &mut Renames::default());
        let mut after = before.clone();
        apply(&mut after, alter, &mut renames);
        let c = changes(&before, &after, &renames);
        assert_eq!(c.tables.iter().find(|(t, _)| t == "people").unwrap().1.mark, Mark::Altered);
    }

    #[test]
    fn change_table_reversible_and_sql() {
        let src = r#"class Tweak < ActiveRecord::Migration[8.0]
  def change
    change_table :users, bulk: true do |t|
      t.remove :admin
      t.rename :name, :full_name
      t.string :locale, default: 'en'
      t.index :locale
    end
    reversible do |dir|
      dir.up do
        execute <<~SQL.squish
          CREATE TABLE audits (id integer PRIMARY KEY, note text)
        SQL
      end
      dir.down { execute "DROP TABLE audits" }
    end
    create_join_table :users, :roles do |t|
      t.index [:user_id, :role_id], unique: true
    end
    up_only { execute "UPDATE users SET locale = 'en'" }
    add_index :users, :email unless index_exists?(:users, :email)
    execute "ALTER TABLE #{table} ADD x int"
  end
end
"#;
        let (s, w) = replay(&[USERS, src]);
        let users = s.table("users").unwrap();
        assert!(users.column("admin").is_none() && users.column("full_name").is_some());
        assert_eq!(users.column("locale").unwrap().default, Some(DefaultValue::String("en".into())));
        assert!(users.indexes.iter().any(|i| i.name == "index_users_on_locale"));
        assert!(s.table("audits").is_some(), "the up side of reversible is applied");
        let join = s.table("roles_users").unwrap();
        assert_eq!(join.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["user_id", "role_id"]);
        assert!(join.indexes[0].unique);
        assert_eq!(w[1].len(), 1, "{:?}", w[1]);
        assert!(w[1][0].message.contains("run time") && w[1][0].line == Some(22), "{:?}", w[1]);
    }

    #[test]
    fn postgres_enums_and_uuid_keys() {
        let src = r#"class Moods < ActiveRecord::Migration[7.1]
  def change
    create_enum :mood, ["happy", "sad"]
    create_table :people, id: :uuid do |t|
      t.enum :current_mood, enum_type: :mood, default: "happy", null: false
    end
  end
end
"#;
        let later = "class More < ActiveRecord::Migration[7.1]\n  def change\n    add_enum_value :mood, \"meh\", after: \"happy\"\n  end\nend\n";
        let (s, w) = replay(&[src, later]);
        assert_eq!(w, vec![vec![], vec![]]);
        let people = s.table("people").unwrap();
        assert_eq!(people.column("id").unwrap().data_type, DataType::Uuid);
        assert_eq!(people.column("current_mood").unwrap().data_type, DataType::Enum(vec!["happy".into(), "meh".into(), "sad".into()]));
    }

    #[test]
    fn unknown_things_warn_with_lines() {
        let src = "class X < ActiveRecord::Migration[8.0]\n  def change\n    add_column :nope, :a, :string\n    create_table :t do |t|\n      t.string :a, frobnicate: true\n      t.virtual :b, type: :string, as: \"a\"\n      t.wibble :c\n    end\n    add_magic :t\n    update_index :t, :a\n  end\nend\n";
        let (_, w) = replay(&[src]);
        let lines: Vec<Option<usize>> = w[0].iter().map(|w| w.line).collect();
        assert_eq!(lines, [Some(3), Some(5), Some(6), Some(7), Some(9), Some(10)], "{:?}", w[0]);
    }

    #[test]
    fn guarded_and_rescued_statements() {
        let src = r#"class Guarded < ActiveRecord::Migration[8.0]
  def up
    begin
      execute "CREATE INDEX index_users_on_name ON users (name)"
    rescue ActiveRecord::StatementInvalid
      remove_index :users, name: "index_users_on_name"
      drop_table :users
    end
    remove_index :users, :nickname if index_exists?(:users, :nickname)
    if index_name_exists?(:users, "old")
      rename_index :users, "old", "new"
    end
    add_column :users, :tags, :string, default: [], array: true
    add_check_constraint :users, "name IS NOT NULL", name: "users_name_null", validate: false
    remove_check_constraint :users, name: "users_name_null"
    add_foreign_key :users, :users, column: :manager_id
    execute <<~SQL.squish
      UPDATE users SET name = '#{name}'
    SQL
    return unless column_exists?(:users, :legacy)

    remove_column :users, :legacy
  end
end
"#;
        let mut s = replay(&[USERS]).0;
        s.tables[0].columns.push(Column::new("manager_id", DataType::BigInt));
        let w = apply(&mut s, src, &mut Renames::default());
        let users = s.table("users").expect("the rescue branch isn't run");
        assert!(users.indexes.iter().any(|i| i.name == "index_users_on_name"));
        assert_eq!(users.column("tags").unwrap().data_type, DataType::Custom("varchar(255)[]".into()));
        assert_eq!(users.column("tags").unwrap().default, Some(DefaultValue::Expr("'{}'".into())));
        let messages: Vec<&str> = w.iter().map(|w| w.message.as_str()).collect();
        assert!(messages.len() == 1 && messages[0].contains("users_name_null"), "{messages:?}");
        // A key Rails named itself is found by its hashed name.
        let drop = format!("ALTER TABLE users DROP CONSTRAINT {}", foreign_key_name("users", "manager_id"));
        assert!(raw_sql::apply(&mut s, &drop, &mut Renames::default()).is_empty());
        assert!(s.table("users").unwrap().column("manager_id").unwrap().references.is_none());
    }

    #[test]
    fn file_names_and_words() {
        // As Rails computes them.
        assert_eq!(foreign_key_name("posts", "user_id"), "fk_rails_5b5ddfd518");
        let long = ["relationship_severance_event_id".to_string()];
        assert_eq!(index_name("account_relationship_severance_events", &long, true), "idx_on_relationship_severance_event_id_403f53e707");
        assert_eq!(index_name("account_relationship_severance_events", &long, false), "index_account_relationship_severance_events_on_relationship_severance_event_id");
        assert_eq!(split_id("20240115093000_create_users"), (Some("2024-01-15".into()), "create users".into()));
        assert_eq!(split_id("20240115093000_create_active_storage_tables.active_storage"), (Some("2024-01-15".into()), "create active storage tables".into()));
        assert_eq!(split_id("001_init"), (None, "init".into()));
        assert_eq!(join_table_name("products", "categories"), "categories_products");
        assert_eq!(join_table_name("music_artists", "music_records"), "music_artists_records");
        for (p, s) in [("users", "user"), ("categories", "category"), ("addresses", "address"), ("boxes", "box"), ("people", "person"), ("statuses", "status"), ("status", "status")] {
            assert_eq!(singular(p), s, "{p}");
        }
    }

    /// What sdb writes as a Rails migration reads back as the schema it came
    /// from. (On SQLite, tables with enums or comments are written as SQL in
    /// SQLite's own text, which `raw_sql` doesn't read.)
    #[test]
    fn reads_back_generated_migrations() {
        let target = crate::parser::parse(crate::STARTER_SCHEMA).unwrap();
        for d in [Dialect::Postgres, Dialect::Mysql] {
            let g = framework::generate(&Schema::default(), &target, d, Framework::Rails, &framework::Options::default());
            let (s, w) = replay(&[&g.code]);
            assert_eq!(w, vec![vec![]], "{d:?}\n{}", g.code);
            let (want, got) = (crate::ddl::canonical(&target, d), crate::ddl::canonical(&s, d));
            for t in &want.tables {
                assert_eq!(got.table(&t.name), Some(t), "{d:?} {}\n{}", t.name, g.code);
            }
            assert_eq!(got.tables.len(), want.tables.len());
        }
    }
}
