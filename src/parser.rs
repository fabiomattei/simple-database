//! Parser for the `.dbs` schema DSL.
//!
//! ```text
//! # comments start with '#' or '//'
//! table users {
//!   id         int pk autoincrement
//!   email      varchar(255) unique not null
//!   role       text not null default 'member' check (role in ('member', 'admin'))
//!   team_id    int -> teams.id on delete cascade
//!   created_at timestamp not null default now
//!
//!   index (team_id)
//!   unique index users_email_role (email, role)
//! }
//! ```

use crate::schema::*;
use std::fmt;

#[derive(Debug)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    /// Contents of a balanced `( ... )` group, verbatim.
    Paren(String),
    Arrow,
    LBrace,
    RBrace,
    /// End of a statement: newline, `,` or `;`.
    End,
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tok::Word(w) => write!(f, "'{w}'"),
            Tok::Str(s) => write!(f, "string '{s}'"),
            Tok::Paren(p) => write!(f, "'({p})'"),
            Tok::Arrow => write!(f, "'->'"),
            Tok::LBrace => write!(f, "'{{'"),
            Tok::RBrace => write!(f, "'}}'"),
            Tok::End => write!(f, "end of line"),
        }
    }
}

fn err<T>(line: usize, message: impl Into<String>) -> Result<T, ParseError> {
    Err(ParseError {
        line,
        message: message.into(),
    })
}

fn tokenize(src: &str) -> Result<Vec<(Tok, usize)>, ParseError> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut line = 1;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                toks.push((Tok::End, line));
                line += 1;
                i += 1;
            }
            ',' | ';' => {
                toks.push((Tok::End, line));
                i += 1;
            }
            c if c.is_whitespace() => i += 1,
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '{' => {
                toks.push((Tok::LBrace, line));
                i += 1;
            }
            '}' => {
                toks.push((Tok::RBrace, line));
                i += 1;
            }
            '-' if chars.get(i + 1) == Some(&'>') => {
                toks.push((Tok::Arrow, line));
                i += 2;
            }
            '\'' | '"' => {
                let quote = c;
                let start_line = line;
                let mut s = String::new();
                i += 1;
                loop {
                    match chars.get(i) {
                        None => return err(start_line, "unterminated string"),
                        Some(&ch) if ch == quote => {
                            // Doubled quote is an escaped quote.
                            if chars.get(i + 1) == Some(&quote) {
                                s.push(quote);
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        }
                        Some(&ch) => {
                            if ch == '\n' {
                                line += 1;
                            }
                            s.push(ch);
                            i += 1;
                        }
                    }
                }
                toks.push((Tok::Str(s), start_line));
            }
            '(' => {
                let start_line = line;
                let mut depth = 0;
                let mut s = String::new();
                let mut in_quote: Option<char> = None;
                loop {
                    let Some(&ch) = chars.get(i) else {
                        return err(start_line, "unbalanced '('");
                    };
                    if ch == '\n' {
                        line += 1;
                    }
                    match (in_quote, ch) {
                        (Some(q), ch) if ch == q => in_quote = None,
                        (Some(_), _) => {}
                        (None, '\'' | '"') => in_quote = Some(ch),
                        (None, '(') => depth += 1,
                        (None, ')') => depth -= 1,
                        _ => {}
                    }
                    s.push(ch);
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
                // Strip the outer parentheses.
                let inner = s[1..s.len() - 1].trim().to_string();
                toks.push((Tok::Paren(inner), start_line));
            }
            c if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' => {
                let mut w = String::new();
                while i < chars.len() {
                    let ch = chars[i];
                    let is_word = ch.is_alphanumeric() || ch == '_' || ch == '.';
                    let is_minus = ch == '-' && w.is_empty() && chars.get(i + 1) != Some(&'>');
                    if is_word || is_minus {
                        w.push(ch);
                        i += 1;
                    } else {
                        break;
                    }
                }
                toks.push((Tok::Word(w), line));
            }
            other => return err(line, format!("unexpected character '{other}'")),
        }
    }
    toks.push((Tok::End, line));
    Ok(toks)
}

struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)].0
    }

    fn peek_at(&self, offset: usize) -> &Tok {
        &self.toks[(self.pos + offset).min(self.toks.len() - 1)].0
    }

    fn line(&self) -> usize {
        self.toks[self.pos.min(self.toks.len() - 1)].1
    }

    fn at_eof(&self) -> bool {
        self.pos >= self.toks.len()
    }

    fn next(&mut self) -> Tok {
        let t = self.peek().clone();
        self.pos += 1;
        t
    }

    fn skip_ends(&mut self) {
        while !self.at_eof() && *self.peek() == Tok::End {
            self.pos += 1;
        }
    }

    fn peek_word(&self) -> Option<String> {
        match self.peek() {
            Tok::Word(w) => Some(w.to_ascii_lowercase()),
            _ => None,
        }
    }

    fn expect_word(&mut self, what: &str) -> Result<String, ParseError> {
        let line = self.line();
        match self.next() {
            Tok::Word(w) => Ok(w),
            other => err(line, format!("expected {what}, found {other}")),
        }
    }

    fn eat_keyword(&mut self, kw: &str) -> bool {
        if self.peek_word().as_deref() == Some(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), ParseError> {
        let line = self.line();
        if self.eat_keyword(kw) {
            Ok(())
        } else {
            err(line, format!("expected '{kw}', found {}", self.peek()))
        }
    }

    fn parse_schema(&mut self) -> Result<Schema, ParseError> {
        let mut schema = Schema::default();
        loop {
            self.skip_ends();
            if self.at_eof() {
                break;
            }
            let line = self.line();
            match self.peek_word().as_deref() {
                Some("table") => {
                    self.pos += 1;
                    let table = self.parse_table()?;
                    if schema.table(&table.name).is_some() {
                        return err(line, format!("table '{}' is defined twice", table.name));
                    }
                    schema.tables.push(table);
                }
                _ => return err(line, format!("expected 'table', found {}", self.peek())),
            }
        }
        Ok(schema)
    }

    fn parse_table(&mut self) -> Result<Table, ParseError> {
        let name = self.expect_word("table name")?;
        let mut was = vec![];
        if self.eat_keyword("was") {
            self.parse_was(&mut was)?;
        }
        let line = self.line();
        if self.next() != Tok::LBrace {
            return err(line, format!("expected '{{' after table name '{name}'"));
        }
        let mut table = Table {
            name,
            columns: vec![],
            composite_pk: vec![],
            indexes: vec![],
            was,
        };
        loop {
            self.skip_ends();
            if self.at_eof() {
                return err(self.line(), format!("missing '}}' for table '{}'", table.name));
            }
            if *self.peek() == Tok::RBrace {
                self.pos += 1;
                break;
            }
            self.parse_table_item(&mut table)?;
            let line = self.line();
            match self.peek() {
                Tok::End | Tok::RBrace => {}
                other => return err(line, format!("unexpected {other}")),
            }
        }
        Ok(table)
    }

    fn parse_table_item(&mut self, table: &mut Table) -> Result<(), ParseError> {
        let line = self.line();
        let first = self.peek_word().unwrap_or_default();
        let second_is_paren = matches!(self.peek_at(1), Tok::Paren(_));

        // Table-level constraints. A column can still be called e.g. "index" as
        // long as it isn't followed by a parenthesised list.
        let is_index = first == "index" && self.looks_like_index();
        let is_unique_index = first == "unique"
            && (second_is_paren || matches!(self.peek_at(1), Tok::Word(w) if w.eq_ignore_ascii_case("index")));
        let is_pk = (first == "pk" && second_is_paren)
            || (first == "primary"
                && matches!(self.peek_at(1), Tok::Word(w) if w.eq_ignore_ascii_case("key"))
                && matches!(self.peek_at(2), Tok::Paren(_)));

        if is_pk {
            self.pos += if first == "pk" { 1 } else { 2 };
            let cols = self.expect_paren_list("primary key columns")?;
            if !table.composite_pk.is_empty() || table.columns.iter().any(|c| c.primary_key) {
                return err(line, "table already has a primary key");
            }
            table.composite_pk = cols;
            return Ok(());
        }

        if is_index || is_unique_index {
            let unique = self.eat_keyword("unique");
            self.eat_keyword("index");
            let explicit_name = match self.peek() {
                Tok::Word(_) => Some(self.expect_word("index name")?),
                _ => None,
            };
            let columns = self.expect_paren_list("index columns")?;
            let name = explicit_name.unwrap_or_else(|| {
                format!(
                    "{}_{}_{}",
                    table.name,
                    columns.join("_"),
                    if unique { "key" } else { "idx" }
                )
            });
            table.indexes.push(Index {
                name,
                columns,
                unique,
            });
            return Ok(());
        }

        let column = self.parse_column()?;
        if table.column(&column.name).is_some() {
            return err(line, format!("column '{}' is defined twice", column.name));
        }
        if column.primary_key && !table.composite_pk.is_empty() {
            return err(line, "table already has a composite primary key");
        }
        table.columns.push(column);
        Ok(())
    }

    /// `index (a, b)` or `index name (a, b)`, as opposed to a column named
    /// "index" such as `index varchar(10)`.
    fn looks_like_index(&self) -> bool {
        match self.peek_at(1) {
            Tok::Paren(_) => true,
            Tok::Word(w) => {
                matches!(self.peek_at(2), Tok::Paren(_))
                    && matches!(DataType::parse(w), DataType::Custom(_))
            }
            _ => false,
        }
    }

    /// The earlier names after `was`: one name or a list `(a, b)`.
    fn parse_was(&mut self, was: &mut Vec<String>) -> Result<(), ParseError> {
        let names = match self.peek() {
            Tok::Paren(_) => self.expect_paren_list("earlier names")?,
            _ => vec![self.expect_word("earlier name after 'was'")?],
        };
        was.extend(names);
        Ok(())
    }

    fn expect_paren_list(&mut self, what: &str) -> Result<Vec<String>, ParseError> {
        let line = self.line();
        match self.next() {
            Tok::Paren(p) => {
                let items: Vec<String> = p
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if items.is_empty() {
                    return err(line, format!("empty list of {what}"));
                }
                Ok(items)
            }
            other => err(line, format!("expected ({what}), found {other}")),
        }
    }

    fn parse_column(&mut self) -> Result<Column, ParseError> {
        let name = self.expect_word("column name")?;
        let mut type_str = self.expect_word(&format!("type for column '{name}'"))?;
        if let Tok::Paren(args) = self.peek().clone() {
            self.pos += 1;
            type_str = format!("{type_str}({args})");
        }
        let mut col = Column::new(name, DataType::parse(&type_str));

        loop {
            let line = self.line();
            match self.peek().clone() {
                Tok::End | Tok::RBrace => break,
                Tok::Arrow => {
                    self.pos += 1;
                    let target = self.expect_word("referenced table.column after '->'")?;
                    let Some((t, c)) = target.split_once('.') else {
                        return err(line, format!("expected 'table.column' after '->', found '{target}'"));
                    };
                    col.references = Some(ForeignKey {
                        table: t.to_string(),
                        column: c.to_string(),
                        on_delete: RefAction::NoAction,
                        on_update: RefAction::NoAction,
                    });
                }
                Tok::Word(w) => {
                    self.pos += 1;
                    match w.to_ascii_lowercase().as_str() {
                        "pk" => col.primary_key = true,
                        "primary" => {
                            self.expect_keyword("key")?;
                            col.primary_key = true;
                        }
                        "autoincrement" | "auto_increment" | "increment" | "serial" => {
                            col.auto_increment = true
                        }
                        "unique" => col.unique = true,
                        "was" => self.parse_was(&mut col.was)?,
                        "not" => {
                            self.expect_keyword("null")?;
                            col.nullable = false;
                        }
                        "notnull" | "required" => col.nullable = false,
                        "null" | "nullable" | "optional" => col.nullable = true,
                        "default" => col.default = Some(self.parse_default()?),
                        "check" => match self.next() {
                            Tok::Paren(expr) => col.check = Some(expr),
                            other => return err(line, format!("expected (expression) after 'check', found {other}")),
                        },
                        "references" | "ref" => {
                            let t = self.expect_word("referenced table")?;
                            let (table, column) = match t.split_once('.') {
                                Some((t, c)) => (t.to_string(), c.to_string()),
                                None => match self.next() {
                                    Tok::Paren(c) => (t, c.trim().to_string()),
                                    other => return err(line, format!("expected (column) after referenced table, found {other}")),
                                },
                            };
                            col.references = Some(ForeignKey {
                                table,
                                column,
                                on_delete: RefAction::NoAction,
                                on_update: RefAction::NoAction,
                            });
                        }
                        "on" => {
                            let event = self.expect_word("'delete' or 'update'")?.to_ascii_lowercase();
                            let mut action = self.expect_word("referential action")?;
                            if matches!(action.to_ascii_lowercase().as_str(), "set" | "no") {
                                action = format!("{action} {}", self.expect_word("referential action")?);
                            }
                            let Some(action) = RefAction::parse(&action) else {
                                return err(line, format!("unknown referential action '{action}' (use cascade, set_null, set_default, restrict, no_action)"));
                            };
                            let Some(fk) = col.references.as_mut() else {
                                return err(line, "'on delete/update' must follow a reference ('-> table.column')");
                            };
                            match event.as_str() {
                                "delete" => fk.on_delete = action,
                                "update" => fk.on_update = action,
                                other => return err(line, format!("expected 'delete' or 'update' after 'on', found '{other}'")),
                            }
                        }
                        other => return err(line, format!("unknown column modifier '{other}' on column '{}'", col.name)),
                    }
                }
                other => return err(line, format!("unexpected {other} in column '{}'", col.name)),
            }
        }

        if col.primary_key {
            col.nullable = false;
        }
        Ok(col)
    }

    fn parse_default(&mut self) -> Result<DefaultValue, ParseError> {
        let line = self.line();
        Ok(match self.next() {
            Tok::Str(s) => DefaultValue::String(s),
            Tok::Paren(e) => DefaultValue::Expr(e),
            Tok::Word(w) => {
                let lower = w.to_ascii_lowercase();
                match lower.as_str() {
                    "null" => DefaultValue::Null,
                    "true" => DefaultValue::Bool(true),
                    "false" => DefaultValue::Bool(false),
                    "now" | "current_timestamp" => {
                        // Allow `now()` as well as `now`.
                        if matches!(self.peek(), Tok::Paren(a) if a.is_empty()) {
                            self.pos += 1;
                        }
                        DefaultValue::Now
                    }
                    _ if w.parse::<f64>().is_ok() => DefaultValue::Number(w),
                    _ => match self.peek().clone() {
                        // A function call such as `gen_random_uuid()`.
                        Tok::Paren(args) => {
                            self.pos += 1;
                            DefaultValue::Expr(format!("{w}({args})"))
                        }
                        _ => DefaultValue::Expr(lower),
                    },
                }
            }
            other => return err(line, format!("expected a default value, found {other}")),
        })
    }
}

pub fn parse(src: &str) -> Result<Schema, ParseError> {
    let toks = tokenize(src)?;
    Parser { toks, pos: 0 }.parse_schema()
}

/// Where a table and its columns are in the source. Lines are 1-based.
#[derive(Debug, Clone, PartialEq)]
pub struct OutlineTable {
    pub name: String,
    pub line: usize,
    /// Line of the closing `}` (the last line when it is missing).
    pub end_line: usize,
    pub columns: Vec<OutlineColumn>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutlineColumn {
    pub name: String,
    pub data_type: String,
    pub line: usize,
}

/// The tables and columns of `src`, found by a lenient scan that keeps
/// working while the text doesn't parse (for editors).
pub fn outline(src: &str) -> Vec<OutlineTable> {
    // (token, line); `None` ends a statement.
    let mut toks: Vec<(Option<String>, usize)> = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let (mut i, mut line) = (0, 1);
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' | ',' | ';' => {
                toks.push((None, line));
                line += usize::from(c == '\n');
                i += 1;
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '{' | '}' => {
                toks.push((None, line));
                toks.push((Some(c.to_string()), line));
                toks.push((None, line));
                i += 1;
            }
            // Strings and groups never span lines here, so a missing quote or
            // `)` only spoils its own line.
            '\'' | '"' | '(' => {
                let start = i;
                let mut depth = 0;
                let mut quote = None;
                while i < chars.len() && chars[i] != '\n' {
                    let ch = chars[i];
                    i += 1;
                    match (quote, ch) {
                        (Some(q), ch) if ch == q => quote = None,
                        (Some(_), _) => {}
                        (None, '\'' | '"') => quote = Some(ch),
                        (None, '(') => depth += 1,
                        (None, ')') => depth -= 1,
                        _ => {}
                    }
                    if quote.is_none() && depth <= 0 {
                        break;
                    }
                }
                let text: String = chars[start..i].iter().collect();
                // A type's arguments belong to it: `varchar(255)`, `decimal (10, 2)`.
                if c == '('
                    && let Some((Some(prev), _)) = toks.last_mut()
                    && prev != "{"
                    && prev != "}"
                {
                    prev.push_str(&text);
                } else {
                    toks.push((Some(text), line));
                }
            }
            c if c.is_whitespace() => i += 1,
            _ => {
                let start = i;
                while i < chars.len() && !chars[i].is_whitespace() && !"#,;{}()'\"".contains(chars[i]) {
                    i += 1;
                }
                toks.push((Some(chars[start..i].iter().collect()), line));
            }
        }
    }

    let mut tables: Vec<OutlineTable> = Vec::new();
    let mut open = false;
    for stmt in toks.split(|(t, _)| t.is_none()) {
        let words: Vec<(&str, usize)> = stmt.iter().filter_map(|(t, l)| t.as_deref().map(|t| (t, *l))).collect();
        let Some(&(first, first_line)) = words.first() else { continue };
        match first {
            "{" => {}
            "}" => {
                if open && let Some(t) = tables.last_mut() {
                    t.end_line = first_line;
                }
                open = false;
            }
            _ if first.eq_ignore_ascii_case("table") => {
                if let Some(&(name, _)) = words.get(1) {
                    tables.push(OutlineTable { name: name.to_string(), line: first_line, end_line: line, columns: vec![] });
                    open = true;
                }
            }
            _ if open => {
                // Table-level constraints, told apart from columns named like them as `parse_table_item` does.
                let lower = first.to_ascii_lowercase();
                let second = words.get(1).map_or(String::new(), |(w, _)| w.to_ascii_lowercase());
                let constraint = lower.starts_with("index(")
                    || lower.starts_with("unique(")
                    || lower.starts_with("pk(")
                    || (lower == "unique" && second.starts_with("index"))
                    || (lower == "primary" && second.starts_with("key("))
                    || (lower == "index"
                        && second.split_once('(').is_some_and(|(ty, _)| matches!(DataType::parse(ty), DataType::Custom(_))));
                if !constraint && !first.starts_with(['(', '\'', '"']) && let Some(t) = tables.last_mut() {
                    t.columns.push(OutlineColumn {
                        name: first.to_string(),
                        data_type: words.get(1).map_or(String::new(), |(w, _)| w.to_string()),
                        line: first_line,
                    });
                }
            }
            _ => {}
        }
    }
    tables
}

/// The line of a `table` or `table.column` location, as `validate` reports them.
pub fn locate(outline: &[OutlineTable], location: &str) -> Option<usize> {
    let (table, column) = match location.split_once('.') {
        Some((t, c)) => (t, Some(c)),
        None => (location, None),
    };
    let t = outline.iter().find(|t| t.name.eq_ignore_ascii_case(table))?;
    Some(column.and_then(|c| t.columns.iter().find(|x| x.name.eq_ignore_ascii_case(c))).map_or(t.line, |c| c.line))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
        # A small blog
        table users {
          id         int pk autoincrement
          email      varchar(255) unique not null
          role       text not null default 'member' check (role in ('member', 'admin'))
          created_at timestamp not null default now
        }

        table posts {
          id        int pk autoincrement
          author_id int not null -> users.id on delete cascade
          title     text not null
          score     decimal(10, 2) default 0
          index (author_id, title)
        }

        table post_tags {
          post_id int references posts(id) on delete set null
          tag     text
          pk (post_id, tag)
        }
    "#;

    #[test]
    fn parses_full_example() {
        let s = parse(SRC).unwrap();
        assert_eq!(s.tables.len(), 3);

        let users = s.table("users").unwrap();
        let id = users.column("id").unwrap();
        assert!(id.primary_key && id.auto_increment && !id.nullable);
        assert_eq!(users.column("email").unwrap().data_type, DataType::Varchar(255));
        let role = users.column("role").unwrap();
        assert_eq!(role.default, Some(DefaultValue::String("member".into())));
        assert_eq!(role.check.as_deref(), Some("role in ('member', 'admin')"));
        assert_eq!(users.column("created_at").unwrap().default, Some(DefaultValue::Now));

        let posts = s.table("posts").unwrap();
        let fk = posts.column("author_id").unwrap().references.as_ref().unwrap();
        assert_eq!((fk.table.as_str(), fk.column.as_str()), ("users", "id"));
        assert_eq!(fk.on_delete, RefAction::Cascade);
        assert_eq!(posts.column("score").unwrap().data_type, DataType::Decimal(10, 2));
        assert_eq!(posts.indexes[0].columns, vec!["author_id", "title"]);
        assert_eq!(posts.indexes[0].name, "posts_author_id_title_idx");

        let pt = s.table("post_tags").unwrap();
        assert_eq!(pt.composite_pk, vec!["post_id", "tag"]);
        let fk = pt.column("post_id").unwrap().references.as_ref().unwrap();
        assert_eq!(fk.on_delete, RefAction::SetNull);
    }

    #[test]
    fn reports_line_numbers() {
        let e = parse("table t {\n  id int\n  name text bogus\n}").unwrap_err();
        assert_eq!(e.line, 3);
        assert!(e.message.contains("bogus"));
    }

    #[test]
    fn rejects_duplicate_columns() {
        let e = parse("table t { a int\n a text }").unwrap_err();
        assert!(e.message.contains("defined twice"));
    }

    #[test]
    fn outline_survives_broken_text() {
        let src = "table users {\n  id int pk\n  name varchar (40) # who\n  index name_idx (name)\n  index varchar(10)\n}\n\ntable posts {\n  author -> users.id, title text\n  body 'unterminated\n  pk (author, title)\n";
        let o = outline(src);
        assert_eq!(o.len(), 2);
        assert_eq!((o[0].name.as_str(), o[0].line, o[0].end_line), ("users", 1, 6));
        let cols: Vec<(&str, &str, usize)> = o[0].columns.iter().map(|c| (c.name.as_str(), c.data_type.as_str(), c.line)).collect();
        assert_eq!(cols, vec![("id", "int", 2), ("name", "varchar(40)", 3), ("index", "varchar(10)", 5)]);
        let cols: Vec<&str> = o[1].columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cols, vec!["author", "title", "body"]);
        assert_eq!(o[1].end_line, 12, "unclosed table runs to the end");
        assert_eq!(locate(&o, "users.name"), Some(3));
        assert_eq!(locate(&o, "posts"), Some(8));
        assert_eq!(locate(&o, "posts.nope"), Some(8));
        assert_eq!(locate(&o, "nope"), None);
    }
}

