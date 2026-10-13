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
        let line = self.line();
        if self.next() != Tok::LBrace {
            return err(line, format!("expected '{{' after table name '{name}'"));
        }
        let mut table = Table {
            name,
            columns: vec![],
            composite_pk: vec![],
            indexes: vec![],
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
}
