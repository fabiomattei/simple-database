//! Just enough Ruby to read a Rails migration: tokens, the body of a method,
//! its statements, and calls (`t.string :title, null: false`,
//! `create_table :posts do |t| ... end`) with literal arguments and options.
//! Anything else is kept as source text, for warnings.
//!
//! One file over the size target, as `php_syntax.rs`: lexing and parsing this
//! small subset is one responsibility.

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Token {
    /// A name or keyword: `t`, `create_table`, `Foo`, `@x`, `valid?`.
    Word(String),
    /// A hash key written `name:`.
    Key(String),
    /// A string, or a symbol (`:posts`) without its colon: Rails takes either.
    Str(String),
    Number(String),
    Newline,
    /// `->`, `=>`, `::`, `&.` or one other character.
    Punct(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Spanned {
    pub token: Token,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Value {
    /// A string or a symbol.
    Str(String),
    Number(String),
    Bool(bool),
    Nil,
    Array(Vec<Value>),
    Hash(Vec<(String, Value)>),
    /// `-> { "CURRENT_TIMESTAMP" }`: SQL as written.
    Raw(String),
    /// Anything else, as source text.
    Other(String),
}

/// `t.string :title, null: false` or `create_table :posts do |t| ... end`.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Call {
    pub receiver: Option<String>,
    pub name: String,
    pub args: Vec<Value>,
    pub options: Vec<(String, Value)>,
    /// The `do |t| ... end` block's body.
    pub block: Option<Vec<Spanned>>,
    pub line: usize,
}

impl Call {
    pub fn option(&self, key: &str) -> Option<&Value> {
        self.options.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Statement {
    pub line: usize,
    pub text: String,
    /// `None` when the statement isn't one call (a condition, a chain, an assignment).
    pub call: Option<Call>,
}

pub(super) fn tokenize(src: &str) -> Vec<Spanned> {
    let chars: Vec<char> = src.chars().collect();
    let mut out: Vec<Spanned> = Vec::new();
    // Heredocs whose body starts on the next line: (token index, label, `<<~`).
    let mut heredocs: Vec<(usize, String, bool)> = Vec::new();
    let (mut i, mut line) = (0, 1);
    while i < chars.len() {
        let (c, next) = (chars[i], chars.get(i + 1).copied());
        let (end, token) = if c == '\n' {
            let mut end = i + 1;
            for (index, label, squiggly) in heredocs.drain(..) {
                let (text, after) = heredoc_body(&chars, end, &label, squiggly);
                out[index].token = Token::Str(text);
                end = after;
            }
            (end, Some(Token::Newline))
        } else if c.is_whitespace() {
            (i + 1, None)
        } else if c == '\\' && next == Some('\n') {
            (i + 2, None)
        } else if c == '#' {
            (chars[i..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| i + p), None)
        } else if c == '\'' || c == '"' {
            let (text, end) = quoted(&chars, i);
            (end, Some(Token::Str(text)))
        } else if let Some((label, squiggly, end)) = heredoc_start(&chars, i) {
            heredocs.push((out.len(), label, squiggly));
            (end, Some(Token::Str(String::new())))
        } else if c == '%' && matches!(next, Some('w' | 'i')) && chars.get(i + 2) == Some(&'[') {
            let close = chars[i..].iter().position(|&c| c == ']').map_or(chars.len(), |p| i + p);
            let words: String = chars[i + 3..close].iter().collect();
            out.push(Spanned { token: Token::Punct("[".into()), line });
            for (n, w) in words.split_whitespace().enumerate() {
                if n > 0 {
                    out.push(Spanned { token: Token::Punct(",".into()), line });
                }
                out.push(Spanned { token: Token::Str(w.into()), line });
            }
            (close + 1, Some(Token::Punct("]".into())))
        } else if c == ':' && next.is_some_and(is_word_start) {
            let end = word_end(&chars, i + 1);
            (end, Some(Token::Str(chars[i + 1..end].iter().collect())))
        } else if c == ':' && next == Some('"') {
            let (text, end) = quoted(&chars, i + 1);
            (end, Some(Token::Str(text)))
        } else if c.is_ascii_digit() {
            let mut end = i;
            while end < chars.len() && (chars[end].is_ascii_digit() || chars[end] == '_' || (chars[end] == '.' && chars.get(end + 1).is_some_and(char::is_ascii_digit))) {
                end += 1;
            }
            (end, Some(Token::Number(chars[i..end].iter().filter(|&&c| c != '_').collect())))
        } else if is_word_start(c) || c == '@' {
            let end = word_end(&chars, i + 1);
            let word: String = chars[i..end].iter().collect();
            match (chars.get(end), chars.get(end + 1)) {
                (Some(':'), next) if next != Some(&':') => (end + 1, Some(Token::Key(word))),
                _ => (end, Some(Token::Word(word))),
            }
        } else {
            let punct = ["->", "=>", "::", "&."].into_iter().find(|p| chars[i..].starts_with(&p.chars().collect::<Vec<_>>())).map_or(c.to_string(), str::to_string);
            (i + punct.chars().count(), Some(Token::Punct(punct)))
        };
        if let Some(token) = token {
            out.push(Spanned { token, line });
        }
        line += chars[i..end].iter().filter(|&&c| c == '\n').count();
        i = end;
    }
    out
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

/// The end of a name starting before `from`, with a trailing `?` or `!`.
fn word_end(chars: &[char], from: usize) -> usize {
    let end = from + chars[from..].iter().take_while(|&&c| c.is_alphanumeric() || c == '_').count();
    if matches!(chars.get(end), Some('?' | '!')) && chars.get(end + 1) != Some(&'=') { end + 1 } else { end }
}

/// A quoted string starting at `start`, and the index after it; `#{...}` stays as written.
fn quoted(chars: &[char], start: usize) -> (String, usize) {
    let quote = chars[start];
    let mut text = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        match (chars[i], chars.get(i + 1).copied()) {
            ('\\', Some(e)) if e == quote || e == '\\' => text.push(e),
            ('\\', Some('n')) if quote == '"' => text.push('\n'),
            ('\\', Some('t')) if quote == '"' => text.push('\t'),
            (c, _) if c == quote => return (text, i + 1),
            (c, _) => {
                text.push(c);
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    (text, chars.len())
}

/// `<<~SQL`, `<<-SQL`, `<<SQL` or a quoted label at `i`: (label, squiggly, index after it).
/// The label must be uppercase or quoted, so `list << item` isn't taken for one.
fn heredoc_start(chars: &[char], i: usize) -> Option<(String, bool, usize)> {
    if chars.get(i) != Some(&'<') || chars.get(i + 1) != Some(&'<') {
        return None;
    }
    let squiggly = chars.get(i + 2) == Some(&'~');
    let start = if matches!(chars.get(i + 2), Some('~' | '-')) { i + 3 } else { i + 2 };
    match chars.get(start)? {
        '\'' | '"' => {
            let (label, end) = quoted(chars, start);
            Some((label, squiggly, end))
        }
        c if c.is_ascii_uppercase() || *c == '_' => {
            let end = start + chars[start..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == '_').count();
            Some((chars[start..end].iter().collect(), squiggly, end))
        }
        _ => None,
    }
}

/// A heredoc's lines from `start` up to its closing label, and the index of
/// the line break after that label. `<<~` removes the common indentation.
fn heredoc_body(chars: &[char], start: usize, label: &str, squiggly: bool) -> (String, usize) {
    let mut lines: Vec<String> = Vec::new();
    let mut i = start;
    let mut end = chars.len();
    while i < chars.len() {
        let line_end = chars[i..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| i + p);
        let text: String = chars[i..line_end].iter().collect();
        if text.trim() == label {
            end = line_end;
            break;
        }
        lines.push(text);
        i = line_end + 1;
    }
    if squiggly {
        let indent = lines.iter().filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
        lines = lines.iter().map(|l| l.chars().skip(indent).collect()).collect();
    }
    (lines.join("\n"), end)
}

fn is(t: &Spanned, punct: &str) -> bool {
    matches!(&t.token, Token::Punct(p) if p == punct)
}

fn is_word(t: &Spanned, word: &str) -> bool {
    matches!(&t.token, Token::Word(w) if w == word)
}

/// Whether the word at `i` opens a block closed by `end`: `do`, `def` and
/// the like, and `if` / `unless` / `while` / `until` at the start of a
/// statement (not as a trailing modifier).
fn is_opener(tokens: &[Spanned], i: usize) -> bool {
    let Token::Word(w) = &tokens[i].token else { return false };
    match w.as_str() {
        "do" | "def" | "class" | "module" | "begin" | "case" => true,
        "if" | "unless" | "while" | "until" => i == 0 || matches!(&tokens[i - 1].token, Token::Newline) || is(&tokens[i - 1], ";") || is(&tokens[i - 1], "="),
        _ => false,
    }
}

/// The index of the `end` that closes the opener at `open`; the last index if unclosed.
fn matching_end(tokens: &[Spanned], open: usize) -> usize {
    let mut depth = 0;
    for i in open..tokens.len() {
        if is_opener(tokens, i) {
            depth += 1;
        } else if is_word(&tokens[i], "end") {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
    }
    tokens.len().saturating_sub(1)
}

/// The index of the bracket that closes the one at `open`; the last index if unclosed.
fn matching(tokens: &[Spanned], open: usize) -> usize {
    let mut depth = 0;
    for (i, t) in tokens.iter().enumerate().skip(open) {
        if ["(", "[", "{"].iter().any(|p| is(t, p)) {
            depth += 1;
        } else if [")", "]", "}"].iter().any(|p| is(t, p)) {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
    }
    tokens.len().saturating_sub(1)
}

/// The tokens between `def <name>` and its `end`.
pub(super) fn method_body<'a>(tokens: &'a [Spanned], name: &str) -> Option<&'a [Spanned]> {
    let start = tokens.windows(2).position(|w| is_word(&w[0], "def") && is_word(&w[1], name))?;
    let close = matching_end(tokens, start);
    let body = (start..close).find(|&i| matches!(tokens[i].token, Token::Newline)).unwrap_or(close);
    Some(&tokens[body..close])
}

/// The statements of a block, in order.
pub(super) fn statements(tokens: &[Spanned]) -> Vec<Statement> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if matches!(tokens[i].token, Token::Newline) || is(&tokens[i], ";") {
            i += 1;
            continue;
        }
        let end = statement_end(tokens, i);
        let body = &tokens[i..end];
        out.push(Statement { line: tokens[i].line, text: source_text(body), call: call(body) });
        i = end;
    }
    out
}

/// The index of the line break or `;` that ends the statement at `start`.
/// A line ending in `,` or `.` goes on, and so does one before a line starting with `.`.
fn statement_end(tokens: &[Spanned], start: usize) -> usize {
    let mut i = start;
    while i < tokens.len() {
        let t = &tokens[i];
        if is_opener(tokens, i) {
            i = matching_end(tokens, i) + 1;
            continue;
        }
        if ["(", "[", "{"].iter().any(|p| is(t, p)) {
            i = matching(tokens, i) + 1;
            continue;
        }
        if is(t, ";") {
            return i;
        }
        if matches!(t.token, Token::Newline) {
            let continued = i > start && ([",", ".", "&."].iter().any(|p| is(&tokens[i - 1], p)));
            let next = tokens[i..].iter().find(|t| !matches!(t.token, Token::Newline));
            if !continued && !next.is_some_and(|n| is(n, ".") || is(n, "&.")) {
                return i;
            }
        }
        i += 1;
    }
    tokens.len()
}

fn call(tokens: &[Spanned]) -> Option<Call> {
    let (receiver, name, line, mut i) = match tokens.iter().map(|t| &t.token).collect::<Vec<_>>().as_slice() {
        [Token::Word(r), Token::Punct(dot), Token::Word(n), ..] if dot == "." => (Some(r.clone()), n.clone(), tokens[2].line, 3),
        [Token::Word(n), ..] => (None, n.clone(), tokens[0].line, 1),
        _ => return None,
    };
    if ["if", "unless", "while", "until", "case", "begin", "def", "class", "module", "return", "end"].contains(&name.as_str()) {
        return None;
    }
    let args: Vec<Spanned>;
    if tokens.get(i).is_some_and(|t| is(t, "(")) {
        let close = matching(tokens, i);
        args = tokens[i + 1..close].to_vec();
        i = close + 1;
    } else {
        let end = (i..tokens.len()).find(|&j| is_word(&tokens[j], "do")).unwrap_or(tokens.len());
        args = tokens[i..end].iter().filter(|t| !matches!(t.token, Token::Newline)).cloned().collect();
        i = end;
    }
    let mut block = None;
    if tokens.get(i).is_some_and(|t| is_word(t, "do")) {
        let close = matching_end(tokens, i);
        let mut body = i + 1;
        if tokens.get(body).is_some_and(|t| is(t, "|")) {
            body = (body + 1..close).find(|&j| is(&tokens[j], "|")).map_or(body, |j| j + 1);
        }
        block = Some(tokens[body..close].to_vec());
        i = close + 1;
    }
    // A trailing `if` / `unless` makes it conditional.
    if i < tokens.len() || args.iter().any(|t| ["if", "unless", "while", "until"].iter().any(|w| is_word(t, w))) {
        return None;
    }
    let mut call = Call { receiver, name, args: Vec::new(), options: Vec::new(), block, line };
    for part in split_commas(&args) {
        match part.split_first() {
            Some((Spanned { token: Token::Key(k), .. }, rest)) => call.options.push((k.clone(), value(rest))),
            _ => match part.iter().position(|t| is(t, "=>")) {
                Some(arrow) if let [Spanned { token: Token::Str(k), .. }] = &part[..arrow] => call.options.push((k.clone(), value(&part[arrow + 1..]))),
                _ => call.args.push(value(part)),
            },
        }
    }
    Some(call)
}

/// The parts of a list separated by top-level commas; a trailing comma adds nothing.
fn split_commas(tokens: &[Spanned]) -> Vec<&[Spanned]> {
    let mut parts = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < tokens.len() {
        if is(&tokens[i], ",") {
            parts.push(&tokens[start..i]);
            start = i + 1;
        } else if ["(", "[", "{"].iter().any(|p| is(&tokens[i], p)) {
            i = matching(tokens, i);
        }
        i += 1;
    }
    if start < tokens.len() {
        parts.push(&tokens[start..]);
    }
    parts
}

fn value(tokens: &[Spanned]) -> Value {
    let tokens: Vec<Spanned> = tokens.iter().filter(|t| !matches!(t.token, Token::Newline)).cloned().collect();
    let last = tokens.len().saturating_sub(1);
    match tokens.iter().map(|t| &t.token).collect::<Vec<_>>().as_slice() {
        [Token::Str(s)] => Value::Str(s.clone()),
        [Token::Number(n)] => Value::Number(n.clone()),
        [Token::Punct(m), Token::Number(n)] if m == "-" => Value::Number(format!("-{n}")),
        [Token::Word(w)] if w == "true" || w == "false" => Value::Bool(w == "true"),
        [Token::Word(w)] if w == "nil" => Value::Nil,
        [Token::Punct(o), ..] if o == "[" && matching(&tokens, 0) == last => Value::Array(split_commas(&tokens[1..last]).into_iter().map(value).collect()),
        [Token::Punct(o), ..] if o == "{" && matching(&tokens, 0) == last => hash(&tokens[1..last]),
        [Token::Punct(l), Token::Punct(o), Token::Str(sql), Token::Punct(c)] if l == "->" && o == "{" && c == "}" => Value::Raw(sql.clone()),
        _ => Value::Other(source_text(&tokens)),
    }
}

fn hash(tokens: &[Spanned]) -> Value {
    let mut entries = Vec::new();
    for part in split_commas(tokens) {
        match part.split_first() {
            Some((Spanned { token: Token::Key(k), .. }, rest)) => entries.push((k.clone(), value(rest))),
            _ => match part.iter().position(|t| is(t, "=>")) {
                Some(arrow) if let [Spanned { token: Token::Str(k), .. }] = &part[..arrow] => entries.push((k.clone(), value(&part[arrow + 1..]))),
                _ => return Value::Other(source_text(tokens)),
            },
        }
    }
    Value::Hash(entries)
}

/// Tokens as Ruby-like text, for warnings.
fn source_text(tokens: &[Spanned]) -> String {
    let mut out = String::new();
    for t in tokens {
        let text = match &t.token {
            Token::Word(w) | Token::Number(w) => w.clone(),
            Token::Key(k) => format!("{k}:"),
            Token::Str(s) => format!("{s:?}"),
            Token::Newline => "; ".into(),
            Token::Punct(p) => p.clone(),
        };
        let word = !matches!(t.token, Token::Punct(_) | Token::Newline);
        if word && !out.is_empty() && !out.ends_with([' ', '(', '[', '.']) {
            out.push(' ');
        }
        out.push_str(&text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(src: &str) -> Vec<Token> {
        tokenize(src).into_iter().map(|t| t.token).collect()
    }

    #[test]
    fn tokenizes_symbols_keys_strings_and_heredocs() {
        use Token::*;
        let src = "t.string :title, null: false # c\nexecute <<~SQL\n  select 1\n    from x\nSQL\n%i[a b]";
        assert_eq!(
            tokens(src),
            vec![
                Word("t".into()),
                Punct(".".into()),
                Word("string".into()),
                Str("title".into()),
                Punct(",".into()),
                Key("null".into()),
                Word("false".into()),
                Newline,
                Word("execute".into()),
                Str("select 1\n  from x".into()),
                Newline,
                Newline,
                Punct("[".into()),
                Str("a".into()),
                Punct(",".into()),
                Str("b".into()),
                Punct("]".into()),
            ]
        );
        assert_eq!(tokenize(src).last().unwrap().line, 6);
    }

    #[test]
    fn reads_the_statements_of_a_method() {
        let src = "class CreatePosts < ActiveRecord::Migration[8.0]\n\
            def change\n\
              create_table :posts, id: :uuid do |t|\n\
                t.references :user, null: false,\n\
                  foreign_key: { to_table: :people, on_delete: :cascade }\n\
                t.decimal(:price, precision: 8, scale: 2)\n\
              end\n\
              add_index :posts, [:a, :b], unique: true if x\n\
              reversible do |dir|\n\
                dir.up { execute 'x' }\n\
              end\n\
            end\n\
          end\n";
        let tokens = tokenize(src);
        let stmts = statements(method_body(&tokens, "change").unwrap());
        assert_eq!(stmts.len(), 3);
        let create = stmts[0].call.as_ref().unwrap();
        assert_eq!((create.name.as_str(), &create.args[..], create.option("id")), ("create_table", &[Value::Str("posts".into())][..], Some(&Value::Str("uuid".into()))));
        let inner = statements(create.block.as_ref().unwrap());
        let reference = inner[0].call.as_ref().unwrap();
        assert_eq!((reference.receiver.as_deref(), reference.name.as_str(), reference.line), (Some("t"), "references", 4));
        assert_eq!(
            reference.option("foreign_key"),
            Some(&Value::Hash(vec![("to_table".into(), Value::Str("people".into())), ("on_delete".into(), Value::Str("cascade".into()))]))
        );
        assert_eq!(inner[1].call.as_ref().unwrap().option("scale"), Some(&Value::Number("2".into())));
        assert_eq!(stmts[1].call, None);
        assert_eq!(stmts[1].line, 8);
        assert_eq!(stmts[2].call.as_ref().unwrap().name, "reversible");
    }

    #[test]
    fn reads_values() {
        let src = "f 'a', -1, nil, true, [1, :b], -> { 'now()' }, x + 1, \"k\" => 2";
        let call = statements(&tokenize(src)).remove(0).call.unwrap();
        assert_eq!(
            call.args,
            vec![
                Value::Str("a".into()),
                Value::Number("-1".into()),
                Value::Nil,
                Value::Bool(true),
                Value::Array(vec![Value::Number("1".into()), Value::Str("b".into())]),
                Value::Raw("now()".into()),
                Value::Other("x+ 1".into()),
            ]
        );
        assert_eq!(call.options, vec![("k".to_string(), Value::Number("2".into()))]);
    }
}
