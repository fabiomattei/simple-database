//! Just enough PHP to read a Laravel migration: tokens, the body of a method,
//! its statements, and call chains (`$table->string('a')->nullable()`) with
//! literal arguments. Anything else is kept as source text, for warnings.
//!
//! One file over the size target: lexing and parsing this small subset is one
//! responsibility, and the two halves share the token type and nothing else.

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Token {
    /// `$table`, without the `$`.
    Variable(String),
    /// A name or keyword; namespaced names keep their backslashes.
    Word(String),
    Str(String),
    Number(String),
    /// `->`, `?->`, `::`, `=>` or one other character.
    Symbol(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Spanned {
    pub token: Token,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Value {
    Str(String),
    Number(String),
    Bool(bool),
    Null,
    /// The values; keys (`'a' => 1`) are dropped.
    Array(Vec<Value>),
    /// `DB::raw('...')` or `new Expression('...')`: SQL as written.
    Raw(String),
    /// `User::class`: the class name without its namespace.
    Class(String),
    /// A closure's body: a `function () { ... }` block, or an `fn () =>` expression.
    Closure(Vec<Spanned>),
    /// Anything else (variables, constants, expressions), as source text.
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Call {
    pub name: String,
    pub args: Vec<Value>,
    pub line: usize,
}

/// `$table->string('a')->nullable()` or `Schema::create(...)`: what the
/// first call is made on, then the calls in order.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Chain {
    pub receiver: String,
    pub calls: Vec<Call>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Statement {
    pub line: usize,
    pub text: String,
    /// `None` when the statement isn't a call chain.
    pub chain: Option<Chain>,
}

pub(super) fn tokenize(src: &str) -> Vec<Spanned> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let (mut i, mut line) = (0, 1);
    while i < chars.len() {
        let (c, next) = (chars[i], chars.get(i + 1).copied());
        let (end, token) = if c.is_whitespace() {
            (i + 1, None)
        } else if (c == '/' && next == Some('/')) || (c == '#' && next != Some('[')) {
            (chars[i..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| i + p), None)
        } else if c == '/' && next == Some('*') {
            (find(&chars, i + 2, "*/").map_or(chars.len(), |p| p + 2), None)
        } else if c == '\'' || c == '"' {
            let (text, end) = quoted(&chars, i);
            (end, Some(Token::Str(text)))
        } else if find(&chars, i, "<<<") == Some(i) {
            let (text, end) = heredoc(&chars, i + 3);
            (end, Some(Token::Str(text)))
        } else if c == '$' && next.is_some_and(is_word_char) {
            let end = word_end(&chars, i + 1);
            (end, Some(Token::Variable(chars[i + 1..end].iter().collect())))
        } else if c.is_ascii_digit() {
            let end = i + chars[i..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == '.' || **c == '_').count();
            (end, Some(Token::Number(chars[i..end].iter().collect())))
        } else if is_word_char(c) || c == '\\' {
            let end = word_end(&chars, i);
            (end, Some(Token::Word(chars[i..end].iter().collect())))
        } else {
            let symbol = ["?->", "->", "::", "=>"].into_iter().find(|s| find(&chars, i, s) == Some(i)).map_or(c.to_string(), str::to_string);
            (i + symbol.chars().count(), Some(Token::Symbol(symbol)))
        };
        if let Some(token) = token {
            out.push(Spanned { token, line });
        }
        line += chars[i..end].iter().filter(|&&c| c == '\n').count();
        i = end;
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn word_end(chars: &[char], from: usize) -> usize {
    from + chars[from..].iter().take_while(|&&c| is_word_char(c) || c == '\\').count()
}

/// Where `needle` next starts at or after `from`.
fn find(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    (from..chars.len()).find(|&i| chars[i..].starts_with(&needle))
}

/// A quoted string starting at `start`, and the index after it. Single quotes
/// only escape `\'` and `\\`; double-quoted variables stay as written.
fn quoted(chars: &[char], start: usize) -> (String, usize) {
    let quote = chars[start];
    let mut text = String::new();
    let mut i = start + 1;
    while i < chars.len() {
        match (chars[i], chars.get(i + 1).copied()) {
            ('\\', Some(e)) if e == quote || e == '\\' => text.push(e),
            ('\\', Some('n')) if quote == '"' => text.push('\n'),
            ('\\', Some('t')) if quote == '"' => text.push('\t'),
            ('\\', Some('$')) if quote == '"' => text.push('$'),
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

/// A heredoc or nowdoc whose label starts at `start` (after `<<<`), and the
/// index after its closing label. The closing label's indentation is removed
/// from every line, as PHP does.
fn heredoc(chars: &[char], start: usize) -> (String, usize) {
    let header_end = chars[start..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| start + p);
    let label: String = chars[start..header_end].iter().filter(|&&c| is_word_char(c)).collect();
    let mut lines: Vec<String> = Vec::new();
    let mut i = header_end + 1;
    while i < chars.len() {
        let end = chars[i..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| i + p);
        let line: String = chars[i..end].iter().collect();
        let trimmed = line.trim_start();
        if !label.is_empty() && trimmed.starts_with(&label) && !trimmed[label.len()..].starts_with(is_word_char) {
            let indent = line.chars().count() - trimmed.chars().count();
            let body: Vec<String> = lines.iter().map(|l| l.chars().skip(indent).collect()).collect();
            return (body.join("\n"), i + indent + label.chars().count());
        }
        lines.push(line);
        i = end + 1;
    }
    (lines.join("\n"), chars.len())
}

fn is(t: &Spanned, symbol: &str) -> bool {
    matches!(&t.token, Token::Symbol(s) if s == symbol)
}

fn is_word(t: &Spanned, word: &str) -> bool {
    matches!(&t.token, Token::Word(w) if w.eq_ignore_ascii_case(word))
}

/// The index of the bracket that closes the one at `open`; the last index if unclosed.
fn matching(tokens: &[Spanned], open: usize) -> usize {
    let mut depth = 0;
    for (i, t) in tokens.iter().enumerate().skip(open) {
        if ["(", "[", "{"].iter().any(|s| is(t, s)) {
            depth += 1;
        } else if [")", "]", "}"].iter().any(|s| is(t, s)) {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
    }
    tokens.len().saturating_sub(1)
}

/// The tokens between the braces of `function <name>(...) { ... }`.
pub(super) fn method_body<'a>(tokens: &'a [Spanned], name: &str) -> Option<&'a [Spanned]> {
    let start = tokens.windows(3).position(|w| is_word(&w[0], "function") && is_word(&w[1], name) && is(&w[2], "("))?;
    let params_end = matching(tokens, start + 2);
    let open = (params_end..tokens.len()).find(|&i| is(&tokens[i], "{"))?;
    Some(&tokens[open + 1..matching(tokens, open)])
}

/// The statements of a block, in order.
pub(super) fn statements(tokens: &[Spanned]) -> Vec<Statement> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if is(&tokens[i], ";") {
            i += 1;
            continue;
        }
        let end = statement_end(tokens, i);
        let body = if is(&tokens[end - 1], ";") { &tokens[i..end - 1] } else { &tokens[i..end] };
        out.push(Statement { line: tokens[i].line, text: source_text(body), chain: chain(body) });
        i = end;
    }
    out
}

/// The index after a statement starting at `start`: after its `;`, or after
/// the block of an `if`, `foreach`, `try` and the like (with its `else`s).
fn statement_end(tokens: &[Spanned], start: usize) -> usize {
    let mut i = start;
    while i < tokens.len() {
        let t = &tokens[i];
        if is(t, ";") {
            return i + 1;
        }
        if is(t, "{") {
            let close = matching(tokens, i);
            let continues = tokens.get(close + 1).is_some_and(|n| ["else", "elseif", "catch", "finally"].iter().any(|w| is_word(n, w)));
            if !continues {
                return close + 1;
            }
            i = close + 1;
            continue;
        }
        i = if is(t, "(") || is(t, "[") { matching(tokens, i) + 1 } else { i + 1 };
    }
    tokens.len()
}

fn chain(tokens: &[Spanned]) -> Option<Chain> {
    let receiver = match &tokens.first()?.token {
        Token::Variable(v) => format!("${v}"),
        Token::Word(w) => w.trim_start_matches('\\').to_string(),
        _ => return None,
    };
    let mut calls = Vec::new();
    let mut i = 1;
    while i < tokens.len() {
        let arrow = ["->", "?->", "::"].iter().any(|s| is(&tokens[i], s));
        let (Some(name), Some(open)) = (tokens.get(i + 1), tokens.get(i + 2)) else { return None };
        let Token::Word(name_text) = &name.token else { return None };
        if !arrow || !is(open, "(") {
            return None;
        }
        let close = matching(tokens, i + 2);
        calls.push(Call { name: name_text.clone(), args: split_commas(&tokens[i + 3..close]).into_iter().map(value).collect(), line: name.line });
        i = close + 1;
    }
    (!calls.is_empty()).then_some(Chain { receiver, calls })
}

/// The parts of a list separated by top-level commas; a trailing comma adds nothing.
fn split_commas(tokens: &[Spanned]) -> Vec<&[Spanned]> {
    let mut parts = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < tokens.len() {
        if is(&tokens[i], ",") {
            parts.push(&tokens[start..i]);
            start = i + 1;
        } else if ["(", "[", "{"].iter().any(|s| is(&tokens[i], s)) {
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
    use Token::*;
    let words: Vec<&str> = tokens.iter().map(|t| if let Word(w) = &t.token { w.as_str() } else { "" }).collect();
    let last = tokens.len().saturating_sub(1);
    match tokens.iter().map(|t| &t.token).collect::<Vec<_>>().as_slice() {
        [Str(s)] => Value::Str(s.clone()),
        [Number(n)] => Value::Number(n.clone()),
        [Symbol(m), Number(n)] if m == "-" => Value::Number(format!("-{n}")),
        [Word(w)] if w.eq_ignore_ascii_case("true") || w.eq_ignore_ascii_case("false") => Value::Bool(w.eq_ignore_ascii_case("true")),
        [Word(w)] if w.eq_ignore_ascii_case("null") => Value::Null,
        [Symbol(o), ..] if o == "[" && matching(tokens, 0) == last => array(&tokens[1..last]),
        [Word(w), Symbol(o), ..] if w.eq_ignore_ascii_case("array") && o == "(" && matching(tokens, 1) == last => array(&tokens[2..last]),
        [Word(class), Symbol(c), Word(k)] if c == "::" && k == "class" => Value::Class(class.rsplit('\\').next().unwrap_or(class).to_string()),
        [Word(db), Symbol(c), Word(raw), Symbol(o), Str(sql), Symbol(e)] if db.trim_start_matches('\\') == "DB" && c == "::" && raw == "raw" && o == "(" && e == ")" => Value::Raw(sql.clone()),
        [Word(new), Word(class), Symbol(o), Str(sql), Symbol(e)] if new == "new" && class.ends_with("Expression") && o == "(" && e == ")" => Value::Raw(sql.clone()),
        _ if words.iter().take(2).any(|w| *w == "function") => match tokens.iter().position(|t| is(t, "{")) {
            Some(open) => Value::Closure(tokens[open + 1..matching(tokens, open)].to_vec()),
            None => Value::Other(source_text(tokens)),
        },
        _ if words.iter().take(2).any(|w| *w == "fn") => match tokens.iter().position(|t| is(t, "=>")) {
            Some(arrow) => Value::Closure(tokens[arrow + 1..].to_vec()),
            None => Value::Other(source_text(tokens)),
        },
        _ => Value::Other(source_text(tokens)),
    }
}

fn array(tokens: &[Spanned]) -> Value {
    let items = split_commas(tokens).into_iter().map(|item| match item.iter().position(|t| is(t, "=>")) {
        Some(arrow) => value(&item[arrow + 1..]),
        None => value(item),
    });
    Value::Array(items.collect())
}

/// Tokens as PHP-like text, for warnings.
fn source_text(tokens: &[Spanned]) -> String {
    let mut out = String::new();
    let mut word_before = false;
    for t in tokens {
        let (text, word) = match &t.token {
            Token::Variable(v) => (format!("${v}"), true),
            Token::Word(w) | Token::Number(w) => (w.clone(), true),
            Token::Str(s) => (format!("'{s}'"), true),
            Token::Symbol(s) => (s.clone(), false),
        };
        if word && word_before {
            out.push(' ');
        }
        out.push_str(&text);
        word_before = word;
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
    fn tokenizes_strings_comments_and_heredocs() {
        let src = "// c\n$t->x('it\\'s', \"a\\\"b\") /* x\ny */ # z\n<<<'SQL'\n    select 1\n    SQL;";
        assert_eq!(
            tokens(src),
            vec![
                Token::Variable("t".into()),
                Token::Symbol("->".into()),
                Token::Word("x".into()),
                Token::Symbol("(".into()),
                Token::Str("it's".into()),
                Token::Symbol(",".into()),
                Token::Str("a\"b".into()),
                Token::Symbol(")".into()),
                Token::Str("select 1".into()),
                Token::Symbol(";".into()),
            ]
        );
        assert_eq!(tokenize("a\n/* \n */\nb").last().unwrap().line, 4);
    }

    #[test]
    fn reads_the_statements_of_a_method() {
        let src = "<?php return new class extends Migration {\n\
            public function up(): void {\n\
                Schema::create('users', function (Blueprint $table) {\n\
                    $table->string('email', 100)->nullable()->default(DB::raw('x'));\n\
                });\n\
                if ($x) { foo(); } else { bar(); }\n\
            }\n\
            public function down(): void { Schema::drop('users'); }\n\
        };";
        let tokens = tokenize(src);
        let body = method_body(&tokens, "up").unwrap();
        let stmts = statements(body);
        assert_eq!(stmts.len(), 2);
        let chain = stmts[0].chain.as_ref().unwrap();
        assert_eq!(chain.receiver, "Schema");
        assert_eq!(chain.calls[0].name, "create");
        assert_eq!(chain.calls[0].args[0], Value::Str("users".into()));
        let Value::Closure(inner) = &chain.calls[0].args[1] else { panic!("not a closure") };
        let column = statements(inner).remove(0).chain.unwrap();
        assert_eq!(column.receiver, "$table");
        assert_eq!(column.calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["string", "nullable", "default"]);
        assert_eq!(column.calls[0].args, vec![Value::Str("email".into()), Value::Number("100".into())]);
        assert_eq!(column.calls[0].line, 4);
        assert_eq!(column.calls[2].args, vec![Value::Raw("x".into())]);
        assert_eq!(stmts[1].chain, None);
        assert_eq!(stmts[1].line, 6);
    }

    #[test]
    fn reads_values() {
        let src = "$t->f(['a', 'b' => 2,], true, null, -1, App\\Models\\User::class, $x . 'y', fn ($t) => $t->id())";
        let stmt = statements(&tokenize(src)).remove(0);
        let args = &stmt.chain.unwrap().calls[0].args;
        assert_eq!(args[0], Value::Array(vec![Value::Str("a".into()), Value::Number("2".into())]));
        assert_eq!(args[1..5], [Value::Bool(true), Value::Null, Value::Number("-1".into()), Value::Class("User".into())]);
        assert_eq!(args[5], Value::Other("$x.'y'".into()));
        assert!(matches!(&args[6], Value::Closure(body) if body.len() == 5));
    }
}
