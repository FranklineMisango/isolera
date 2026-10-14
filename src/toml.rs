//! A small, dependency-free TOML reader.
//!
//! Isolera's configuration surface is deliberately narrow: tables, dotted keys,
//! strings, integers, floats, booleans, arrays and inline tables. That subset is
//! implemented here completely - including multi-line arrays, quoted keys,
//! comments, `[[array-of-tables]]` and `\uXXXX` escapes - because pulling in a
//! general TOML crate would add a dependency tree to a root-privileged tool.
//!
//! Table keys are kept in a `BTreeMap`, so iteration (and therefore generated
//! configuration) is deterministic across runs.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::error::{Error, Kind, Result};

/// A parsed TOML table.
pub type Table = BTreeMap<String, Value>;

/// A parsed TOML value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Quoted string (basic or literal).
    String(String),
    /// 64-bit integer.
    Integer(i64),
    /// Floating point number.
    Float(f64),
    /// Boolean.
    Bool(bool),
    /// Homogeneous-or-not array.
    Array(Vec<Value>),
    /// Nested table (``[a.b]`` or an inline ``{ }`` table).
    Table(Table),
}

impl Value {
    /// TOML type name, used in error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Value::String(_) => "string",
            Value::Integer(_) => "integer",
            Value::Float(_) => "float",
            Value::Bool(_) => "boolean",
            Value::Array(_) => "array",
            Value::Table(_) => "table",
        }
    }

    /// Borrow as a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(text) => Some(text),
            _ => None,
        }
    }

    /// Borrow as an integer, accepting a float with no fractional part.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Integer(number) => Some(*number),
            Value::Float(number) if number.fract() == 0.0 => Some(*number as i64),
            _ => None,
        }
    }

    /// Borrow as a float, promoting integers.
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(number) => Some(*number),
            Value::Integer(number) => Some(*number as f64),
            _ => None,
        }
    }

    /// Borrow as a boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(flag) => Some(*flag),
            _ => None,
        }
    }

    /// Borrow as an array.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Borrow as a table.
    pub fn as_table(&self) -> Option<&Table> {
        match self {
            Value::Table(table) => Some(table),
            _ => None,
        }
    }

    /// Render the value as TOML source (sorted keys for tables).
    pub fn to_toml(&self) -> String {
        let mut out = String::new();
        write_value(&mut out, self);
        out
    }
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::String(text) => {
            out.push('"');
            for character in text.chars() {
                match character {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\t' => out.push_str("\\t"),
                    '\r' => out.push_str("\\r"),
                    character => out.push(character),
                }
            }
            out.push('"');
        }
        Value::Integer(number) => {
            let _ = write!(out, "{number}");
        }
        Value::Float(number) => {
            let _ = write!(out, "{number}");
            if !out.ends_with('0') && number.fract() == 0.0 {
                out.push_str(".0");
            }
        }
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Array(items) => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                if position > 0 {
                    out.push_str(", ");
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Table(table) => {
            out.push_str("{ ");
            for (position, (key, item)) in table.iter().enumerate() {
                if position > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{key} = ");
                write_value(out, item);
            }
            out.push_str(" }");
        }
    }
}

/// Parse a TOML document into a [`Table`].
pub fn parse(text: &str) -> Result<Table> {
    Parser {
        chars: text.chars().collect(),
        pos: 0,
    }
    .parse_document()
}

/// Serialise a table as a TOML document with ``[section]`` headers.
///
/// Keys are emitted in sorted order (the table is a `BTreeMap`), so a generated
/// file is byte-identical across runs - important when the file is committed or
/// diffed during a change review.
pub fn to_document(table: &Table) -> String {
    let mut out = String::new();
    write_document(&mut out, &[], table);
    out
}

fn is_array_of_tables(value: &Value) -> bool {
    match value {
        Value::Array(items) => {
            !items.is_empty() && items.iter().all(|item| matches!(item, Value::Table(_)))
        }
        _ => false,
    }
}

fn write_document(out: &mut String, path: &[String], table: &Table) {
    for (key, value) in table {
        if matches!(value, Value::Table(_)) || is_array_of_tables(value) {
            continue;
        }
        out.push_str(&format!("{key} = {}\n", value.to_toml()));
    }
    for (key, value) in table {
        let mut child = path.to_vec();
        child.push(key.clone());
        let header = child.join(".");
        match value {
            Value::Table(sub) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[{header}]\n"));
                write_document(out, &child, sub);
            }
            Value::Array(items) if is_array_of_tables(value) => {
                for item in items {
                    if let Value::Table(sub) = item {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(&format!("[[{header}]]\n"));
                        write_document(out, &child, sub);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Parse a TOML document, or explain why it could not be parsed.
pub fn parse_named(text: &str, origin: &str) -> Result<Table> {
    parse(text).map_err(|err| Error::new(err.kind(), format!("{origin}: {}", err.message())))
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek();
        if character.is_some() {
            self.pos += 1;
        }
        character
    }

    fn starts_with(&self, literal: &str) -> bool {
        literal
            .chars()
            .enumerate()
            .all(|(offset, expected)| self.chars.get(self.pos + offset) == Some(&expected))
    }

    fn line_number(&self) -> usize {
        1 + self.chars[..self.pos.min(self.chars.len())]
            .iter()
            .filter(|character| **character == '\n')
            .count()
    }

    fn fail<T>(&self, message: impl AsRef<str>) -> Result<T> {
        Err(Error::new(
            Kind::Config,
            format!("TOML line {}: {}", self.line_number(), message.as_ref()),
        ))
    }

    fn skip_inline_ws(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t') | Some('\r')) {
            self.pos += 1;
        }
    }

    /// Skip whitespace, newlines and comments.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(' ') | Some('\t') | Some('\r') | Some('\n') => self.pos += 1,
                Some('#') => {
                    while let Some(character) = self.peek() {
                        if character == '\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => return,
            }
        }
    }

    fn expect(&mut self, expected: char) -> Result<()> {
        match self.peek() {
            Some(character) if character == expected => {
                self.pos += 1;
                Ok(())
            }
            Some(character) => self.fail(format!("expected {expected:?} but found {character:?}")),
            None => self.fail(format!("expected {expected:?} but reached end of file")),
        }
    }

    /// Parse the whole document.
    fn parse_document(mut self) -> Result<Table> {
        let mut root = Table::new();
        let mut header: Vec<String> = Vec::new();
        loop {
            self.skip_trivia();
            if self.peek().is_none() {
                return Ok(root);
            }
            if self.peek() == Some('[') {
                self.pos += 1;
                let is_array = self.peek() == Some('[');
                if is_array {
                    self.pos += 1;
                }
                let path = self.parse_key_path()?;
                self.skip_inline_ws();
                self.expect(']')?;
                if is_array {
                    self.expect(']')?;
                }
                header = path;
                if is_array {
                    array_of_tables(&mut root, &header, self.line_number())?;
                } else {
                    navigate(&mut root, &header, self.line_number())?;
                }
                self.expect_statement_end()?;
                continue;
            }

            let keys = self.parse_key_path()?;
            self.skip_inline_ws();
            self.expect('=')?;
            self.skip_inline_ws();
            let value = self.parse_value()?;
            let mut full = header.clone();
            full.extend(keys);
            insert_value(&mut root, &full, value, self.line_number())?;
            self.expect_statement_end()?;
        }
    }

    /// Consume the end of a key/value statement: optional comment, then newline.
    fn expect_statement_end(&mut self) -> Result<()> {
        self.skip_inline_ws();
        if self.peek() == Some('#') {
            while let Some(character) = self.peek() {
                if character == '\n' {
                    break;
                }
                self.pos += 1;
            }
        }
        match self.peek() {
            None => Ok(()),
            Some('\n') => {
                self.pos += 1;
                Ok(())
            }
            Some(character) => self.fail(format!("unexpected {character:?} after value")),
        }
    }

    /// Parse a dotted key path, allowing bare and quoted segments.
    fn parse_key_path(&mut self) -> Result<Vec<String>> {
        let mut segments = Vec::new();
        loop {
            self.skip_inline_ws();
            segments.push(self.parse_key_segment()?);
            self.skip_inline_ws();
            if self.peek() == Some('.') {
                self.pos += 1;
                continue;
            }
            return Ok(segments);
        }
    }

    fn parse_key_segment(&mut self) -> Result<String> {
        match self.peek() {
            Some('"') => self.parse_basic_string(),
            Some('\'') => self.parse_literal_string(),
            Some(character)
                if character.is_alphanumeric() || character == '_' || character == '-' =>
            {
                let start = self.pos;
                while let Some(character) = self.peek() {
                    if character.is_alphanumeric() || character == '_' || character == '-' {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                Ok(self.chars[start..self.pos].iter().collect())
            }
            Some(character) => self.fail(format!("invalid key character {character:?}")),
            None => self.fail("expected a key but reached end of file"),
        }
    }

    /// Parse any TOML value.
    fn parse_value(&mut self) -> Result<Value> {
        match self.peek() {
            Some('"') => self.parse_basic_string().map(Value::String),
            Some('\'') => self.parse_literal_string().map(Value::String),
            Some('[') => self.parse_array(),
            Some('{') => self.parse_inline_table(),
            Some(_) if self.starts_with("true") => {
                self.pos += 4;
                Ok(Value::Bool(true))
            }
            Some(_) if self.starts_with("false") => {
                self.pos += 5;
                Ok(Value::Bool(false))
            }
            Some(character)
                if character.is_ascii_digit()
                    || character == '-'
                    || character == '+'
                    || character == 'i'
                    || character == 'n' =>
            {
                self.parse_number()
            }
            Some(character) => self.fail(format!("unexpected value {character:?}")),
            None => self.fail("expected a value but reached end of file"),
        }
    }

    fn parse_basic_string(&mut self) -> Result<String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return self.fail("unterminated string"),
                Some('"') => return Ok(out),
                Some('\n') => return self.fail("newline inside a single-line string"),
                Some('\\') => match self.bump() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('0') => out.push('\0'),
                    Some('u') => out.push(self.parse_unicode_escape(4)?),
                    Some('U') => out.push(self.parse_unicode_escape(8)?),
                    Some(other) => return self.fail(format!("unknown escape \\{other}")),
                    None => return self.fail("unterminated escape sequence"),
                },
                Some(character) => out.push(character),
            }
        }
    }

    fn parse_unicode_escape(&mut self, digits: usize) -> Result<char> {
        let mut code = 0u32;
        for _ in 0..digits {
            match self.bump() {
                Some(character) => {
                    let digit = character.to_digit(16).ok_or_else(|| {
                        Error::new(
                            Kind::Config,
                            format!(
                                "TOML line {}: invalid hex digit {character:?}",
                                self.line_number()
                            ),
                        )
                    })?;
                    code = code * 16 + digit;
                }
                None => return self.fail("truncated unicode escape"),
            }
        }
        char::from_u32(code).ok_or_else(|| {
            Error::new(
                Kind::Config,
                format!(
                    "TOML line {}: invalid code point U+{code:04X}",
                    self.line_number()
                ),
            )
        })
    }

    fn parse_literal_string(&mut self) -> Result<String> {
        self.expect('\'')?;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return self.fail("unterminated literal string"),
                Some('\'') => return Ok(out),
                Some(character) => out.push(character),
            }
        }
    }

    fn parse_array(&mut self) -> Result<Value> {
        self.expect('[')?;
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                Some(']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                None => return self.fail("unterminated array"),
                _ => {
                    items.push(self.parse_value()?);
                    self.skip_trivia();
                    match self.peek() {
                        Some(',') => self.pos += 1,
                        Some(']') => {}
                        Some(character) => {
                            return self.fail(format!("unexpected {character:?} in array"))
                        }
                        None => return self.fail("unterminated array"),
                    }
                }
            }
        }
    }

    fn parse_inline_table(&mut self) -> Result<Value> {
        self.expect('{')?;
        let mut table = Table::new();
        loop {
            self.skip_trivia();
            if self.peek() == Some('}') {
                self.pos += 1;
                return Ok(Value::Table(table));
            }
            let keys = self.parse_key_path()?;
            self.skip_inline_ws();
            self.expect('=')?;
            self.skip_inline_ws();
            let value = self.parse_value()?;
            insert_value(&mut table, &keys, value, self.line_number())?;
            self.skip_trivia();
            match self.peek() {
                Some(',') => self.pos += 1,
                Some('}') => {}
                Some(character) => {
                    return self.fail(format!("unexpected {character:?} in inline table"))
                }
                None => return self.fail("unterminated inline table"),
            }
        }
    }

    fn parse_number(&mut self) -> Result<Value> {
        let start = self.pos;
        while let Some(character) = self.peek() {
            if character.is_ascii_alphanumeric()
                || character == '_'
                || character == '+'
                || character == '-'
                || character == '.'
            {
                self.pos += 1;
            } else {
                break;
            }
        }
        let raw: String = self.chars[start..self.pos].iter().collect();
        let cleaned: String = raw.chars().filter(|character| *character != '_').collect();
        let lowercase = cleaned.to_ascii_lowercase();
        match lowercase.as_str() {
            "inf" | "+inf" => return Ok(Value::Float(f64::INFINITY)),
            "-inf" => return Ok(Value::Float(f64::NEG_INFINITY)),
            "nan" | "+nan" | "-nan" => return Ok(Value::Float(f64::NAN)),
            _ => {}
        }
        if let Some(rest) = lowercase.strip_prefix("0x") {
            return match i64::from_str_radix(rest, 16) {
                Ok(number) => Ok(Value::Integer(number)),
                Err(_) => self.fail(format!("invalid hexadecimal integer {raw:?}")),
            };
        }
        if let Some(rest) = lowercase.strip_prefix("0o") {
            return match i64::from_str_radix(rest, 8) {
                Ok(number) => Ok(Value::Integer(number)),
                Err(_) => self.fail(format!("invalid octal integer {raw:?}")),
            };
        }
        if let Some(rest) = lowercase.strip_prefix("0b") {
            return match i64::from_str_radix(rest, 2) {
                Ok(number) => Ok(Value::Integer(number)),
                Err(_) => self.fail(format!("invalid binary integer {raw:?}")),
            };
        }
        if lowercase.contains('.') || lowercase.contains('e') {
            return match cleaned.parse::<f64>() {
                Ok(number) => Ok(Value::Float(number)),
                Err(_) => self.fail(format!("invalid float {raw:?}")),
            };
        }
        match cleaned.parse::<i64>() {
            Ok(number) => Ok(Value::Integer(number)),
            Err(_) => self.fail(format!("invalid value {raw:?}")),
        }
    }
}

fn duplicate_key(line: usize, key: &str) -> Error {
    Error::new(
        Kind::Config,
        format!("TOML line {line}: duplicate key {key:?}"),
    )
}

fn type_conflict(line: usize, key: &str, found: &str) -> Error {
    Error::new(
        Kind::Config,
        format!("TOML line {line}: {key:?} is a {found}, not a table"),
    )
}

/// Walk (creating as needed) the table at `path`.
///
/// A path segment that names an array of tables resolves to its last element,
/// which is what TOML requires for ``[[table]]`` followed by ``[table.child]``.
fn navigate<'t>(table: &'t mut Table, path: &[String], line: usize) -> Result<&'t mut Table> {
    let Some((head, rest)) = path.split_first() else {
        return Ok(table);
    };
    let entry = table
        .entry(head.clone())
        .or_insert_with(|| Value::Table(Table::new()));
    match entry {
        Value::Table(sub) => {
            if rest.is_empty() {
                Ok(sub)
            } else {
                navigate(sub, rest, line)
            }
        }
        Value::Array(items) => match items.last_mut() {
            Some(Value::Table(sub)) => {
                if rest.is_empty() {
                    Ok(sub)
                } else {
                    navigate(sub, rest, line)
                }
            }
            _ => Err(type_conflict(line, head, "array")),
        },
        other => Err(type_conflict(line, head, other.kind())),
    }
}

/// Create a new element for an ``[[array-of-tables]]`` header.
fn array_of_tables(root: &mut Table, path: &[String], line: usize) -> Result<()> {
    let Some((last, parents)) = path.split_last() else {
        return Err(Error::new(
            Kind::Config,
            "TOML: empty array-of-tables header",
        ));
    };
    let parent = navigate(root, parents, line)?;
    let entry = parent
        .entry(last.clone())
        .or_insert_with(|| Value::Array(Vec::new()));
    match entry {
        Value::Array(items) => {
            items.push(Value::Table(Table::new()));
            Ok(())
        }
        other => Err(type_conflict(line, last, other.kind())),
    }
}

/// Insert `value` at `path`, rejecting duplicate keys.
fn insert_value(table: &mut Table, path: &[String], value: Value, line: usize) -> Result<()> {
    let Some((last, parents)) = path.split_last() else {
        return Err(Error::new(Kind::Config, "TOML: empty key"));
    };
    let target = navigate(table, parents, line)?;
    if target.contains_key(last) {
        return Err(duplicate_key(line, last));
    }
    target.insert(last.clone(), value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'t>(table: &'t Table, path: &[&str]) -> &'t Value {
        let mut current = table;
        for (position, key) in path.iter().enumerate() {
            let value = &current[*key];
            if position == path.len() - 1 {
                return value;
            }
            current = value.as_table().expect("intermediate value is a table");
        }
        unreachable!("empty path")
    }

    #[test]
    fn parses_tables_and_scalars() {
        let text = r#"
# isolate two cores for the matching engine
[isolation]
cpus = "2-3"
smt = false
ring = 0
budget_us = 150

[grub]
cmdline = "quiet skew_tick=1"
"#;
        let table = parse(text).unwrap();
        assert_eq!(get(&table, &["isolation", "cpus"]).as_str(), Some("2-3"));
        assert_eq!(get(&table, &["isolation", "smt"]).as_bool(), Some(false));
        assert_eq!(get(&table, &["isolation", "ring"]).as_int(), Some(0));
        assert_eq!(
            get(&table, &["isolation", "budget_us"]).as_float(),
            Some(150.0)
        );
        assert_eq!(
            get(&table, &["grub", "cmdline"]).as_str(),
            Some("quiet skew_tick=1")
        );
    }

    #[test]
    fn parses_multiline_arrays_and_comments() {
        let text = r#"
irq_exclude = [
  "nvme.*",   # storage completion queues
  "eth0-rx-2",
]
"#;
        let table = parse(text).unwrap();
        let items = table["irq_exclude"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].as_str(), Some("nvme.*"));
        assert_eq!(items[1].as_str(), Some("eth0-rx-2"));
    }

    #[test]
    fn parses_dotted_and_inline_keys() {
        let text = r#"
[pin]
queues = { feed = "4", orders = "5" }
engine.name = "matching"
engine.core = 6
"#;
        let table = parse(text).unwrap();
        assert_eq!(get(&table, &["pin", "queues", "feed"]).as_str(), Some("4"));
        assert_eq!(
            get(&table, &["pin", "engine", "name"]).as_str(),
            Some("matching")
        );
        assert_eq!(get(&table, &["pin", "engine", "core"]).as_int(), Some(6));
    }

    #[test]
    fn parses_numbers_in_every_base() {
        let table = parse(
            r#"
a = 1_000_000
b = -42
c = 0x1f
d = 0o17
e = 0b1010
f = 1.5
g = 3e3
h = inf
"#,
        )
        .unwrap();
        assert_eq!(table["a"].as_int(), Some(1_000_000));
        assert_eq!(table["b"].as_int(), Some(-42));
        assert_eq!(table["c"].as_int(), Some(31));
        assert_eq!(table["d"].as_int(), Some(15));
        assert_eq!(table["e"].as_int(), Some(10));
        assert_eq!(table["f"].as_float(), Some(1.5));
        assert_eq!(table["g"].as_float(), Some(3000.0));
        assert_eq!(table["h"].as_float(), Some(f64::INFINITY));
    }

    #[test]
    fn parses_string_escapes_and_literals() {
        let table = parse(
            r#"
basic = "tab\there\nsecond \u00e9"
literal = 'C:\path\no\escape'
quoted = "he said \"go\""
"#,
        )
        .unwrap();
        assert_eq!(table["basic"].as_str(), Some("tab\there\nsecond \u{e9}"));
        assert_eq!(table["literal"].as_str(), Some(r"C:\path\no\escape"));
        assert_eq!(table["quoted"].as_str(), Some("he said \"go\""));
    }

    #[test]
    fn parses_quoted_keys_and_array_of_tables() {
        let text = r#"
["weird key"]
value = 1

[[stage]]
name = "boot"
timeout_ms = 250

[[stage]]
name = "steady"
timeout_ms = 500
"#;
        let table = parse(text).unwrap();
        assert_eq!(get(&table, &["weird key", "value"]).as_int(), Some(1));
        let stages = table["stage"].as_array().unwrap();
        assert_eq!(stages.len(), 2);
        assert_eq!(
            stages[1].as_table().unwrap()["name"].as_str(),
            Some("steady")
        );
    }

    #[test]
    fn rejects_duplicate_keys_and_malformed_input() {
        assert!(parse("a = 1\na = 2\n").is_err());
        assert!(parse("a = ").is_err());
        assert!(parse("a = \"unterminated\n").is_err());
        assert!(parse("[a\nb = 1\n").is_err());
        assert!(parse("a = 1 b = 2\n").is_err());
        assert!(parse("a.b = 1\na.b = 2\n").is_err());
    }

    #[test]
    fn errors_carry_line_numbers() {
        let err = parse("[ok]\nx = 1\n\nbad = @\n").unwrap_err();
        assert!(
            err.message().contains("line 4"),
            "unexpected message: {}",
            err.message()
        );
    }

    #[test]
    fn empty_document_is_an_empty_table() {
        assert!(parse("").unwrap().is_empty());
        assert!(parse("# only a comment\n").unwrap().is_empty());
    }

    #[test]
    fn round_trips_through_serialisation() {
        let original = parse(
            r#"
[isolation]
cpus = "2-3"
smt = false
exclude = ["a", "b"]

[nested.inner]
depth = 2
"#,
        )
        .unwrap();
        let rendered = to_document(&original);
        let reparsed = parse(&rendered).unwrap();
        assert_eq!(reparsed, original);
        assert!(rendered.contains("[isolation]"));
        assert!(rendered.contains("[nested.inner]"));
    }

    #[test]
    fn parse_named_prefixes_the_origin() {
        let err = parse_named("a = ", "/etc/isolera.toml").unwrap_err();
        assert!(err.message().starts_with("/etc/isolera.toml:"));
    }
}
