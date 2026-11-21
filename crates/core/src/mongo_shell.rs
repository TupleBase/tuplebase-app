//! The mongo shell commands TupleBase understands, and their parser.
//!
//! A small, fixed subset of the shell: `db.<collection>.<method>(...)` with relaxed JSON
//! arguments and the shell's value helpers (`ObjectId()`, `ISODate()`, ...). Nothing is
//! evaluated as JavaScript: anything outside the grammar is an error with its position.
//!
//! A leaf module, like `config`: it imports nothing from the rest of `core`. `safety`
//! checks the parsed [`Command`] for the read-only guard and `db/mongo` runs it, so both
//! read the same tree, and the guard checks exactly the documents that are sent.
//!
//! Splitting a script into statements ([`split`]) reads strings, comments and regular
//! expressions with the same scanner as the parser.

use std::fmt;
use std::ops::Range;
use std::str::FromStr;

use mongodb::bson::oid::ObjectId;
use mongodb::bson::raw::CString;
use mongodb::bson::{Binary, Bson, DateTime, Decimal128, Document, Regex, Timestamp, Uuid};

/// The collection a command works on. `database` is `None` for the connection's own.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub database: Option<String>,
    pub collection: String,
}

/// One shell command.
// Short-lived, a few per run, so boxing the larger variant would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `show dbs`
    ShowDatabases,
    /// `show collections`, `db.getCollectionNames()`
    ShowCollections { database: Option<String> },
    /// `db.runCommand({...})`: any command the server knows. Never run read-only.
    RunCommand {
        database: Option<String>,
        command: Document,
    },
    /// `db.<collection>.<method>(...)`
    Collection { target: Target, op: Op },
}

/// Options of `aggregate` the shell accepts. Only these, so nothing unknown reaches the
/// server through the options document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AggregateOptions {
    pub allow_disk_use: Option<bool>,
    pub max_time_ms: Option<u64>,
    pub batch_size: Option<u32>,
}

/// What a command does to its collection.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Find {
        filter: Document,
        projection: Option<Document>,
        sort: Option<Document>,
        limit: Option<i64>,
        skip: Option<u64>,
    },
    FindOne {
        filter: Document,
        projection: Option<Document>,
    },
    Aggregate {
        pipeline: Vec<Document>,
        options: AggregateOptions,
    },
    CountDocuments {
        filter: Document,
    },
    EstimatedDocumentCount,
    Distinct {
        field: String,
        filter: Document,
    },
    GetIndexes,
    /// `.explain()` after a `find` or an `aggregate`.
    Explain(Box<Op>),
    InsertOne {
        document: Document,
    },
    InsertMany {
        documents: Vec<Document>,
    },
    /// `updateOne` and `updateMany`. `update` is a document or a pipeline.
    Update {
        many: bool,
        filter: Document,
        update: Bson,
        upsert: Option<bool>,
    },
    ReplaceOne {
        filter: Document,
        replacement: Document,
        upsert: Option<bool>,
    },
    Delete {
        many: bool,
        filter: Document,
    },
    CreateIndex {
        keys: Document,
        options: Option<Document>,
    },
    /// By name, or by its key document.
    DropIndex {
        index: Bson,
    },
    Drop,
}

/// A read, and nothing else: what the read-only path may run. It has no variant that
/// writes, and each maps to one typed driver call that only reads.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadOp {
    ListDatabases,
    ListCollections {
        database: Option<String>,
    },
    Find {
        target: Target,
        filter: Document,
        projection: Option<Document>,
        sort: Option<Document>,
        limit: Option<i64>,
        skip: Option<u64>,
    },
    FindOne {
        target: Target,
        filter: Document,
        projection: Option<Document>,
    },
    Aggregate {
        target: Target,
        pipeline: Vec<Document>,
        options: AggregateOptions,
    },
    CountDocuments {
        target: Target,
        filter: Document,
    },
    EstimatedDocumentCount {
        target: Target,
    },
    Distinct {
        target: Target,
        field: String,
        filter: Document,
    },
    ListIndexes {
        target: Target,
    },
    /// The query planner's view of a find or an aggregate; it never runs the query.
    Explain(Box<ReadOp>),
}

/// Keys that write or run JavaScript wherever they appear. [`Command::as_read`] refuses
/// any command holding one, or any key starting with `$_` (the server's internal
/// operators, such as `$_internalJsEmit`), a second check besides the guard's allowlist
/// in `safety`.
const NEVER_READ: [&str; 5] = ["$out", "$merge", "$where", "$function", "$accumulator"];

impl Command {
    /// The command as a read, or `None` if it may write: a write method, `runCommand`,
    /// or any argument holding a key from [`NEVER_READ`] or JavaScript code.
    pub fn as_read(&self) -> Option<ReadOp> {
        if let Command::Collection { target, .. } = self
            && target.collection.contains('$')
        {
            return None;
        }
        self.as_read_unguarded().filter(read_is_clean)
    }

    /// The command as a read if its method only reads, whatever its arguments hold.
    /// Only for a connection that may write, which runs a `$where` find like any other;
    /// read-only work uses [`Command::as_read`].
    pub fn as_read_unguarded(&self) -> Option<ReadOp> {
        Some(match self {
            Command::ShowDatabases => ReadOp::ListDatabases,
            Command::ShowCollections { database } => ReadOp::ListCollections {
                database: database.clone(),
            },
            Command::RunCommand { .. } => return None,
            Command::Collection { target, op } => op_as_read(target, op)?,
        })
    }
}

fn op_as_read(target: &Target, op: &Op) -> Option<ReadOp> {
    let target = target.clone();
    Some(match op.clone() {
        Op::Find {
            filter,
            projection,
            sort,
            limit,
            skip,
        } => ReadOp::Find {
            target,
            filter,
            projection,
            sort,
            limit,
            skip,
        },
        Op::FindOne { filter, projection } => ReadOp::FindOne {
            target,
            filter,
            projection,
        },
        Op::Aggregate { pipeline, options } => ReadOp::Aggregate {
            target,
            pipeline,
            options,
        },
        Op::CountDocuments { filter } => ReadOp::CountDocuments { target, filter },
        Op::EstimatedDocumentCount => ReadOp::EstimatedDocumentCount { target },
        Op::Distinct { field, filter } => ReadOp::Distinct {
            target,
            field,
            filter,
        },
        Op::GetIndexes => ReadOp::ListIndexes { target },
        Op::Explain(inner) => match op_as_read(&target, &inner)? {
            read @ (ReadOp::Find { .. } | ReadOp::Aggregate { .. }) => {
                ReadOp::Explain(Box::new(read))
            }
            _ => return None,
        },
        Op::InsertOne { .. }
        | Op::InsertMany { .. }
        | Op::Update { .. }
        | Op::ReplaceOne { .. }
        | Op::Delete { .. }
        | Op::CreateIndex { .. }
        | Op::DropIndex { .. }
        | Op::Drop => return None,
    })
}

/// Whether no document in the read holds a [`NEVER_READ`] key or JavaScript code.
fn read_is_clean(read: &ReadOp) -> bool {
    let docs: Vec<&Document> = match read {
        ReadOp::ListDatabases
        | ReadOp::ListCollections { .. }
        | ReadOp::EstimatedDocumentCount { .. }
        | ReadOp::ListIndexes { .. } => Vec::new(),
        ReadOp::Find {
            filter,
            projection,
            sort,
            ..
        } => [Some(filter), projection.as_ref(), sort.as_ref()]
            .into_iter()
            .flatten()
            .collect(),
        ReadOp::FindOne {
            filter, projection, ..
        } => [Some(filter), projection.as_ref()]
            .into_iter()
            .flatten()
            .collect(),
        ReadOp::Aggregate {
            pipeline, options, ..
        } => {
            if options.allow_disk_use == Some(true) {
                return false;
            }
            pipeline.iter().collect()
        }
        ReadOp::CountDocuments { filter, .. } | ReadOp::Distinct { filter, .. } => vec![filter],
        ReadOp::Explain(inner) => return read_is_clean(inner),
    };
    docs.into_iter()
        .all(|doc| bson_is_clean(&Bson::Document(doc.clone())))
}

fn bson_is_clean(value: &Bson) -> bool {
    match value {
        Bson::Document(doc) => doc.iter().all(|(key, value)| {
            !NEVER_READ.contains(&key.as_str()) && !key.starts_with("$_") && bson_is_clean(value)
        }),
        Bson::Array(items) => items.iter().all(bson_is_clean),
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => false,
        _ => true,
    }
}

/// A parse error, with where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// Byte offset into the parsed text.
    pub offset: usize,
    /// 1-based.
    pub line: usize,
    /// 1-based, in characters.
    pub column: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, column {}: {}",
            self.line, self.column, self.message
        )
    }
}

impl std::error::Error for ParseError {}

// The scanner, shared by splitting and parsing.

/// What the scanner found at one place in the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Whitespace; `true` when it holds a line break.
    Space(bool),
    Comment,
    /// `'...'`, `"..."` or `` `...` ``, with backslash escapes.
    Str,
    /// `/pattern/flags`, only where a value may start.
    Regex,
    /// Letters, digits, `_` and `$`, starting with a letter, `_` or `$`.
    Word,
    /// Digits, with an optional fraction and exponent.
    Number,
    /// One of `.(){}[],:;-`; and anything else, which the parser refuses.
    Punct(char),
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// The token at byte `i` of `src` and its length. `value_may_start` says whether a `/`
/// here opens a regular expression rather than being a stray character.
fn scan(src: &str, i: usize, value_may_start: bool) -> (Kind, usize) {
    let rest = &src[i..];
    let c = rest.chars().next().expect("scan inside the text");
    if c.is_whitespace() {
        let len = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        return (Kind::Space(rest[..len].contains(['\n', '\r'])), len);
    }
    if rest.starts_with("//") {
        return (Kind::Comment, rest.find(['\n', '\r']).unwrap_or(rest.len()));
    }
    if rest.starts_with("/*") {
        return (Kind::Comment, rest.find("*/").map_or(rest.len(), |e| e + 2));
    }
    match c {
        '\'' | '"' | '`' => (Kind::Str, string_len(rest, c)),
        '/' if value_may_start => match regex_len(rest) {
            Some(len) => (Kind::Regex, len),
            None => (Kind::Punct('/'), 1),
        },
        c if is_word_start(c) => (
            Kind::Word,
            rest.find(|c: char| !is_word_char(c)).unwrap_or(rest.len()),
        ),
        c if c.is_ascii_digit() => (Kind::Number, number_len(rest)),
        c => (Kind::Punct(c), c.len_utf8()),
    }
}

/// Bytes up to and including the closing quote; to the end when there is none.
fn string_len(text: &str, quote: char) -> usize {
    let mut chars = text.char_indices().skip(1);
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c == quote {
            return i + 1;
        }
    }
    text.len()
}

/// A regular expression's length with its flags, or `None` when no `/` closes it on the
/// same line. A `/` inside `[...]` doesn't close it.
fn regex_len(text: &str) -> Option<usize> {
    let mut chars = text.char_indices().skip(1);
    let mut class = false;
    while let Some((i, c)) = chars.next() {
        match c {
            '\n' | '\r' => return None,
            '\\' => {
                chars.next();
            }
            '[' => class = true,
            ']' => class = false,
            '/' if !class => {
                if i == 1 {
                    return None;
                }
                let flags = text[i + 1..]
                    .find(|c: char| !c.is_ascii_alphabetic())
                    .unwrap_or(text.len() - i - 1);
                return Some(i + 1 + flags);
            }
            _ => {}
        }
    }
    None
}

fn number_len(text: &str) -> usize {
    let b = text.as_bytes();
    let digits = |from: usize| from + b[from..].iter().take_while(|b| b.is_ascii_digit()).count();
    let mut i = digits(0);
    if b.get(i) == Some(&b'.') && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i = digits(i + 1);
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(b.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        if b.get(j).is_some_and(u8::is_ascii_digit) {
            i = digits(j);
        }
    }
    i
}

/// Whether a value may start after a token of this kind: at the start, or after an
/// opening bracket, a comma, a colon or a semicolon.
fn opens_value(prev: Option<Kind>) -> bool {
    matches!(
        prev,
        None | Some(Kind::Punct('(' | '[' | '{' | ',' | ':' | ';'))
    )
}

/// Each token of `src` with its byte range, comments and spaces included.
fn tokens(src: &str) -> impl Iterator<Item = (Kind, Range<usize>)> + '_ {
    let mut i = 0;
    let mut prev: Option<Kind> = None;
    std::iter::from_fn(move || {
        (i < src.len()).then(|| {
            let (kind, len) = scan(src, i, opens_value(prev));
            let range = i..i + len;
            i += len;
            if !matches!(kind, Kind::Space(_) | Kind::Comment) {
                prev = Some(kind);
            }
            (kind, range)
        })
    })
}

/// One statement of a script: its text, trimmed, without its `;`, and where it ends,
/// just past its `;` or at the end of its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub text: Range<usize>,
    pub end: usize,
}

/// Cuts a script into statements. One ends at a `;` outside brackets, or at a line break
/// outside brackets unless the next code (past spaces and comments) starts with `.` or
/// the code before ends with one, so a chain of calls can run over several lines.
/// Comment-only pieces are dropped. Strings, comments and regular expressions are read
/// as the parser reads them, so neither can hide a statement boundary from the other.
pub fn split(src: &str) -> Vec<Piece> {
    let toks: Vec<(Kind, Range<usize>)> = tokens(src).collect();
    let mut pieces = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    let mut has_code = false;
    let mut last_code: Option<Kind> = None;
    let push = |pieces: &mut Vec<Piece>, from: usize, to: usize, end: Option<usize>| {
        let raw = &src[from..to];
        let text_start = from + raw.len() - raw.trim_start().len();
        let text_end = (from + raw.trim_end().len()).max(text_start);
        pieces.push(Piece {
            text: text_start..text_end,
            end: end.unwrap_or(text_end),
        });
    };
    for (index, (kind, range)) in toks.iter().enumerate() {
        match kind {
            Kind::Punct(';') if depth == 0 => {
                if has_code {
                    push(&mut pieces, start, range.start, Some(range.end));
                }
                start = range.end;
                has_code = false;
                last_code = None;
            }
            Kind::Space(true) if depth == 0 && has_code => {
                let next = toks[index + 1..]
                    .iter()
                    .find(|(k, _)| !matches!(k, Kind::Space(_) | Kind::Comment))
                    .map(|(k, _)| *k);
                let continues = next == Some(Kind::Punct('.'))
                    || last_code == Some(Kind::Punct('.'))
                    || next.is_none();
                if !continues {
                    push(&mut pieces, start, range.start, None);
                    start = range.start;
                    has_code = false;
                    last_code = None;
                }
            }
            Kind::Space(_) | Kind::Comment => {}
            _ => {
                match kind {
                    Kind::Punct('(' | '[' | '{') => depth += 1,
                    Kind::Punct(')' | ']' | '}') => depth = depth.saturating_sub(1),
                    _ => {}
                }
                has_code = true;
                last_code = Some(*kind);
            }
        }
    }
    if has_code {
        push(&mut pieces, start, src.len(), None);
    }
    pieces
}

// The parser.

/// A token the parser reads, with its byte range.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Number(String),
    Regex(String, String),
    Punct(char),
}

/// Parses exactly one command, with an optional `;` after it. A second command, or
/// anything that isn't one, is an error with its position.
pub fn parse_command(src: &str) -> Result<Command, ParseError> {
    let mut parser = Parser::new(src)?;
    let command = parser.command()?;
    if parser.peek_punct(';') {
        parser.pos += 1;
    }
    if let Some((_, range)) = parser.toks.get(parser.pos) {
        return Err(parser.error_at(
            range.start,
            "expected the end of the command: run one command at a time, or end each with `;`",
        ));
    }
    Ok(command)
}

/// Parses each statement of a script, cut by [`split`].
pub fn parse_script(src: &str) -> Result<Vec<Command>, ParseError> {
    split(src)
        .into_iter()
        .map(|piece| {
            parse_command(&src[piece.text.clone()]).map_err(|e| {
                // Positions in the whole script.
                let offset = piece.text.start + e.offset;
                let (line, column) = line_column(src, offset);
                ParseError {
                    offset,
                    line,
                    column,
                    message: e.message,
                }
            })
        })
        .collect()
}

fn line_column(src: &str, offset: usize) -> (usize, usize) {
    let before = &src[..offset.min(src.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

/// The collection methods that read, as the parser reads them after `db.<collection>.`;
/// the editor's completions offer these names.
pub const READ_METHODS: [&str; 7] = [
    "find",
    "findOne",
    "aggregate",
    "countDocuments",
    "estimatedDocumentCount",
    "distinct",
    "getIndexes",
];

/// The collection methods that write, run only on a writable connection.
pub const WRITE_METHODS: [&str; 10] = [
    "insertOne",
    "insertMany",
    "updateOne",
    "updateMany",
    "replaceOne",
    "deleteOne",
    "deleteMany",
    "createIndex",
    "dropIndex",
    "drop",
];

/// The methods the parser reads right after `db.`, besides a collection's name.
pub const DATABASE_METHODS: [&str; 5] = [
    "getCollection",
    "getSiblingDB",
    "getCollectionNames",
    "runCommand",
    "adminCommand",
];

/// The methods that may follow `find(...)`.
pub const FIND_CURSOR_METHODS: [&str; 7] = [
    "sort",
    "projection",
    "limit",
    "skip",
    "pretty",
    "toArray",
    "explain",
];

/// The methods that may follow `aggregate(...)`.
pub const AGGREGATE_CURSOR_METHODS: [&str; 3] = ["pretty", "toArray", "explain"];

/// The deepest nesting of documents and arrays accepted, so a hostile input can't
/// exhaust the stack.
const MAX_DEPTH: usize = 100;

struct Parser<'a> {
    src: &'a str,
    toks: Vec<(Tok, Range<usize>)>,
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Result<Self, ParseError> {
        let mut toks = Vec::new();
        let mut parser = Parser {
            src,
            toks: Vec::new(),
            pos: 0,
            depth: 0,
        };
        for (kind, range) in tokens(src) {
            let text = &src[range.clone()];
            let tok = match kind {
                Kind::Space(_) | Kind::Comment => {
                    let open_block =
                        text.starts_with("/*") && (text.len() < 4 || !text.ends_with("*/"));
                    if open_block {
                        return Err(parser.error_at(range.start, "unterminated comment"));
                    }
                    continue;
                }
                Kind::Word => Tok::Word(text.to_owned()),
                Kind::Number => Tok::Number(text.to_owned()),
                Kind::Str => Tok::Str(parser.unescape(text, range.start)?),
                Kind::Regex => {
                    let close = text.rfind('/').expect("a regex ends with /");
                    let pattern = text[1..close].to_owned();
                    let flags = text[close + 1..].to_owned();
                    if let Some(bad) = flags.chars().find(|c| !"imsxu".contains(*c)) {
                        return Err(parser.error_at(
                            range.start + close + 1,
                            &format!("unknown regular expression flag `{bad}`"),
                        ));
                    }
                    Tok::Regex(pattern, flags)
                }
                Kind::Punct(c) => Tok::Punct(c),
            };
            toks.push((tok, range));
        }
        parser.toks = toks;
        Ok(parser)
    }

    fn error_at(&self, offset: usize, message: &str) -> ParseError {
        let (line, column) = line_column(self.src, offset);
        ParseError {
            offset,
            line,
            column,
            message: message.to_owned(),
        }
    }

    /// An error at the current token, or at the end.
    fn error(&self, message: &str) -> ParseError {
        let offset = self
            .toks
            .get(self.pos)
            .map_or(self.src.len(), |(_, r)| r.start);
        self.error_at(offset, message)
    }

    /// The text of a string token without its quotes, with escapes decoded.
    fn unescape(&self, text: &str, at: usize) -> Result<String, ParseError> {
        let quote = text.chars().next().expect("a quote");
        if text.len() < 2 || !text.ends_with(quote) || string_len(text, quote) != text.len() {
            return Err(self.error_at(at, "unterminated string"));
        }
        let inner = &text[1..text.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.char_indices();
        while let Some((i, c)) = chars.next() {
            if c != '\\' {
                if quote == '`' && c == '$' && inner[i..].starts_with("${") {
                    return Err(self.error_at(at + 1 + i, "template expressions are not supported"));
                }
                out.push(c);
                continue;
            }
            let Some((_, e)) = chars.next() else {
                return Err(self.error_at(at + 1 + i, "unterminated escape"));
            };
            match e {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'v' => out.push('\u{b}'),
                '0' => out.push('\0'),
                '\n' => {}
                'x' => {
                    let hex: String = chars.by_ref().take(2).map(|(_, c)| c).collect();
                    let code = u32::from_str_radix(&hex, 16)
                        .ok()
                        .filter(|_| hex.len() == 2)
                        .ok_or_else(|| self.error_at(at + 1 + i, "bad \\x escape"))?;
                    out.push(char::from_u32(code).expect("two hex digits are a char"));
                }
                'u' => {
                    let code = self.unicode_escape(&mut chars, at + 1 + i)?;
                    out.push(code);
                }
                other => out.push(other),
            }
        }
        Ok(out)
    }

    /// `\uXXXX`, `\u{X...}`, and a surrogate pair of two `\uXXXX`.
    fn unicode_escape(
        &self,
        chars: &mut std::str::CharIndices<'_>,
        at: usize,
    ) -> Result<char, ParseError> {
        let bad = || self.error_at(at, "bad \\u escape");
        let read4 = |chars: &mut std::str::CharIndices<'_>| -> Option<u32> {
            let hex: String = chars.by_ref().take(4).map(|(_, c)| c).collect();
            (hex.len() == 4)
                .then(|| u32::from_str_radix(&hex, 16).ok())
                .flatten()
        };
        let rest = chars.as_str();
        if rest.starts_with('{') {
            let close = rest.find('}').ok_or_else(bad)?;
            let code = u32::from_str_radix(&rest[1..close], 16).map_err(|_| bad())?;
            for _ in 0..=close {
                chars.next();
            }
            return char::from_u32(code).ok_or_else(bad);
        }
        let high = read4(chars).ok_or_else(bad)?;
        if (0xD800..0xDC00).contains(&high) {
            if !chars.as_str().starts_with("\\u") {
                return Err(bad());
            }
            chars.next();
            chars.next();
            let low = read4(chars).ok_or_else(bad)?;
            if !(0xDC00..0xE000).contains(&low) {
                return Err(bad());
            }
            let code = 0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00);
            return char::from_u32(code).ok_or_else(bad);
        }
        char::from_u32(high).ok_or_else(bad)
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(t, _)| t)
    }

    fn peek_punct(&self, c: char) -> bool {
        self.peek() == Some(&Tok::Punct(c))
    }

    fn peek_word(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w == word)
    }

    fn next(&mut self) -> Option<Tok> {
        let tok = self.toks.get(self.pos).map(|(t, _)| t.clone());
        self.pos += 1;
        tok
    }

    fn expect_punct(&mut self, c: char) -> Result<(), ParseError> {
        if self.peek_punct(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error(&format!("expected `{c}`")))
        }
    }

    fn word(&mut self, what: &str) -> Result<String, ParseError> {
        match self.peek() {
            Some(Tok::Word(w)) => {
                let w = w.clone();
                self.pos += 1;
                Ok(w)
            }
            _ => Err(self.error(&format!("expected {what}"))),
        }
    }

    fn string(&mut self, what: &str) -> Result<String, ParseError> {
        match self.peek() {
            Some(Tok::Str(s)) => {
                let s = s.clone();
                self.pos += 1;
                Ok(s)
            }
            _ => Err(self.error(&format!("expected {what} as a string"))),
        }
    }

    fn command(&mut self) -> Result<Command, ParseError> {
        if self.peek_word("show") {
            self.pos += 1;
            let what = self.word("`dbs` or `collections`")?;
            return match what.as_str() {
                "dbs" | "databases" => Ok(Command::ShowDatabases),
                "collections" | "tables" => Ok(Command::ShowCollections { database: None }),
                _ => Err(self.error_at(
                    self.toks[self.pos - 1].1.start,
                    "expected `show dbs` or `show collections`",
                )),
            };
        }
        if self.peek_word("use") {
            return Err(self.error("`use` is not supported: use db.getSiblingDB(\"name\")"));
        }
        if !self.peek_word("db") {
            return Err(self.error("expected a command starting with `db.` or `show`"));
        }
        self.pos += 1;
        self.db_chain(None)
    }

    /// After `db`, or after `db.getSiblingDB("name")`.
    fn db_chain(&mut self, database: Option<String>) -> Result<Command, ParseError> {
        self.expect_punct('.')?;
        // A collection's name may hold dots: `db.system.views.find()`. The name runs up
        // to the word followed by `(`.
        let mut parts = Vec::new();
        loop {
            let at = self.pos;
            let word = self.word("a collection or method name")?;
            if self.peek_punct('(') {
                if parts.is_empty() {
                    match word.as_str() {
                        "getSiblingDB" => {
                            let name = self.single_string_arg("a database name")?;
                            return self.db_chain(Some(name));
                        }
                        "getCollection" => {
                            let name = self.single_string_arg("a collection name")?;
                            self.expect_punct('.')?;
                            let target = Target {
                                database,
                                collection: name,
                            };
                            let method = self.word("a method name")?;
                            return self.collection_method(target, &method);
                        }
                        "getCollectionNames" => {
                            self.no_args()?;
                            return Ok(Command::ShowCollections { database });
                        }
                        "runCommand" | "adminCommand" => {
                            let admin = word == "adminCommand";
                            self.expect_punct('(')?;
                            let command = self.document()?;
                            self.expect_punct(')')?;
                            let database = if admin {
                                Some("admin".to_owned())
                            } else {
                                database
                            };
                            return Ok(Command::RunCommand { database, command });
                        }
                        _ => {
                            return Err(self.error_at(
                                self.toks[at].1.start,
                                &format!("unknown database method `{word}`"),
                            ));
                        }
                    }
                }
                let target = Target {
                    database,
                    collection: parts.join("."),
                };
                return self.collection_method(target, &word);
            }
            parts.push(word);
            if !self.peek_punct('.') {
                return Err(self.error("expected `.` and a method call"));
            }
            self.pos += 1;
        }
    }

    fn single_string_arg(&mut self, what: &str) -> Result<String, ParseError> {
        self.expect_punct('(')?;
        let s = self.string(what)?;
        self.expect_punct(')')?;
        Ok(s)
    }

    fn no_args(&mut self) -> Result<(), ParseError> {
        self.expect_punct('(')?;
        self.expect_punct(')')
    }

    /// The arguments in `( ... )`: values separated by commas, a trailing comma allowed.
    fn args(&mut self) -> Result<Vec<(Bson, usize)>, ParseError> {
        self.expect_punct('(')?;
        let mut args = Vec::new();
        while !self.peek_punct(')') {
            let at = self
                .toks
                .get(self.pos)
                .map_or(self.src.len(), |t| t.1.start);
            args.push((self.value()?, at));
            if !self.peek_punct(',') {
                break;
            }
            self.pos += 1;
        }
        self.expect_punct(')')?;
        Ok(args)
    }

    fn collection_method(&mut self, target: Target, method: &str) -> Result<Command, ParseError> {
        let method_at = self.toks[self.pos - 1].1.start;
        let args = self.args()?;
        let mut args = Args {
            parser: self,
            args,
            next: 0,
            method,
            method_at,
        };
        let op = match method {
            "find" => {
                let filter = args.optional_doc("filter")?.unwrap_or_default();
                let projection = args.optional_doc("projection")?;
                args.done()?;
                let mut op = Op::Find {
                    filter,
                    projection,
                    sort: None,
                    limit: None,
                    skip: None,
                };
                self.find_chain(&mut op)?
            }
            "findOne" => {
                let filter = args.optional_doc("filter")?.unwrap_or_default();
                let projection = args.optional_doc("projection")?;
                args.done()?;
                Op::FindOne { filter, projection }
            }
            "aggregate" => {
                let pipeline = args.docs("pipeline")?;
                let options = match args.optional_doc("options")? {
                    None => AggregateOptions::default(),
                    Some(doc) => aggregate_options(doc).map_err(|m| self_error(&args, &m))?,
                };
                args.done()?;
                let op = Op::Aggregate { pipeline, options };
                self.cursor_chain(op)?
            }
            "countDocuments" => {
                let filter = args.optional_doc("filter")?.unwrap_or_default();
                args.done()?;
                Op::CountDocuments { filter }
            }
            "estimatedDocumentCount" => {
                args.done()?;
                Op::EstimatedDocumentCount
            }
            "distinct" => {
                let field = args.string("field")?;
                let filter = args.optional_doc("filter")?.unwrap_or_default();
                args.done()?;
                Op::Distinct { field, filter }
            }
            "getIndexes" => {
                args.done()?;
                Op::GetIndexes
            }
            "insertOne" => {
                let document = args.doc("document")?;
                args.done()?;
                Op::InsertOne { document }
            }
            "insertMany" => {
                let documents = args.docs("documents")?;
                args.done()?;
                Op::InsertMany { documents }
            }
            "updateOne" | "updateMany" => {
                let filter = args.doc("filter")?;
                let update = args.value("update")?;
                if !matches!(update, Bson::Document(_) | Bson::Array(_)) {
                    return Err(self_error(
                        &args,
                        "the update must be a document or a pipeline",
                    ));
                }
                let upsert = args.upsert()?;
                args.done()?;
                Op::Update {
                    many: method == "updateMany",
                    filter,
                    update,
                    upsert,
                }
            }
            "replaceOne" => {
                let filter = args.doc("filter")?;
                let replacement = args.doc("replacement")?;
                let upsert = args.upsert()?;
                args.done()?;
                Op::ReplaceOne {
                    filter,
                    replacement,
                    upsert,
                }
            }
            "deleteOne" | "deleteMany" => {
                let filter = args.doc("filter")?;
                args.done()?;
                Op::Delete {
                    many: method == "deleteMany",
                    filter,
                }
            }
            "createIndex" => {
                let keys = args.doc("keys")?;
                let options = args.optional_doc("options")?;
                args.done()?;
                Op::CreateIndex { keys, options }
            }
            "dropIndex" => {
                let index = args.value("index")?;
                if !matches!(index, Bson::String(_) | Bson::Document(_)) {
                    return Err(self_error(
                        &args,
                        "the index must be a name or a key document",
                    ));
                }
                args.done()?;
                Op::DropIndex { index }
            }
            "drop" => {
                args.done()?;
                Op::Drop
            }
            other => {
                return Err(self.error_at(
                    method_at,
                    &format!("unknown or unsupported collection method `{other}`"),
                ));
            }
        };
        Ok(Command::Collection { target, op })
    }

    /// `.sort()`, `.limit()`, `.skip()`, `.projection()` and `.explain()` after `find`.
    fn find_chain(&mut self, op: &mut Op) -> Result<Op, ParseError> {
        while self.peek_punct('.') {
            self.pos += 1;
            let method = self.word("a cursor method")?;
            let method_at = self.toks[self.pos - 1].1.start;
            if !FIND_CURSOR_METHODS.contains(&method.as_str()) {
                return Err(self.error_at(
                    method_at,
                    &format!("unknown or unsupported cursor method `{method}`"),
                ));
            }
            let args = self.args()?;
            let mut args = Args {
                parser: self,
                args,
                next: 0,
                method: &method,
                method_at,
            };
            let Op::Find {
                projection,
                sort,
                limit,
                skip,
                ..
            } = op
            else {
                unreachable!("only a find has a find chain")
            };
            match method.as_str() {
                "sort" => *sort = Some(args.doc("sort")?),
                "projection" => *projection = Some(args.doc("projection")?),
                "limit" => *limit = Some(args.integer("limit")?),
                "skip" => {
                    let n = args.integer("skip")?;
                    *skip = Some(
                        u64::try_from(n)
                            .map_err(|_| self_error(&args, "skip can't be negative"))?,
                    );
                }
                "pretty" | "toArray" => {}
                "explain" => {
                    // Only the plan: a verbosity that runs the query isn't offered.
                    args.done()?;
                    let inner = std::mem::replace(op, Op::Drop);
                    return self.after_explain(Op::Explain(Box::new(inner)));
                }
                other => {
                    return Err(self.error_at(
                        method_at,
                        &format!("unknown or unsupported cursor method `{other}`"),
                    ));
                }
            }
            args.done()?;
        }
        Ok(std::mem::replace(op, Op::Drop))
    }

    /// `.pretty()`, `.toArray()` and `.explain()` after `aggregate`.
    fn cursor_chain(&mut self, op: Op) -> Result<Op, ParseError> {
        let mut op = op;
        while self.peek_punct('.') {
            self.pos += 1;
            let method = self.word("a cursor method")?;
            let method_at = self.toks[self.pos - 1].1.start;
            self.no_args()?;
            match method.as_str() {
                "pretty" | "toArray" => {}
                "explain" => {
                    op = Op::Explain(Box::new(op));
                    return self.after_explain(op);
                }
                other => {
                    return Err(self.error_at(
                        method_at,
                        &format!("unknown or unsupported cursor method `{other}`"),
                    ));
                }
            }
        }
        Ok(op)
    }

    /// Nothing may follow `.explain()`.
    fn after_explain(&mut self, op: Op) -> Result<Op, ParseError> {
        if self.peek_punct('.') {
            return Err(self.error("nothing can follow .explain()"));
        }
        Ok(op)
    }

    fn document(&mut self) -> Result<Document, ParseError> {
        match self.value()? {
            Bson::Document(doc) => Ok(doc),
            _ => Err(self.error_at(
                self.toks[self.pos - 1].1.start,
                "expected a document `{...}`",
            )),
        }
    }

    fn value(&mut self) -> Result<Bson, ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        let value = self.value_inner();
        self.depth -= 1;
        value
    }

    fn value_inner(&mut self) -> Result<Bson, ParseError> {
        let at = self
            .toks
            .get(self.pos)
            .map_or(self.src.len(), |t| t.1.start);
        let Some(tok) = self.next() else {
            return Err(self.error_at(at, "expected a value"));
        };
        match tok {
            Tok::Punct('{') => self.object(at),
            Tok::Punct('[') => {
                let mut items = Vec::new();
                while !self.peek_punct(']') {
                    items.push(self.value()?);
                    if !self.peek_punct(',') {
                        break;
                    }
                    self.pos += 1;
                }
                self.expect_punct(']')?;
                Ok(Bson::Array(items))
            }
            Tok::Str(s) => Ok(Bson::String(s)),
            Tok::Number(n) => number(&n, false).ok_or_else(|| self.error_at(at, "bad number")),
            Tok::Punct('-') => match self.next() {
                Some(Tok::Number(n)) => {
                    number(&n, true).ok_or_else(|| self.error_at(at, "bad number"))
                }
                Some(Tok::Word(w)) if w == "Infinity" => Ok(Bson::Double(f64::NEG_INFINITY)),
                _ => Err(self.error_at(at, "expected a number after `-`")),
            },
            Tok::Regex(pattern, flags) => regex(&pattern, &flags)
                .ok_or_else(|| self.error_at(at, "a regular expression can't hold a NUL")),
            Tok::Word(w) => self.word_value(&w, at),
            Tok::Punct(c) => Err(self.error_at(at, &format!("unexpected `{c}`"))),
        }
    }

    fn object(&mut self, open: usize) -> Result<Bson, ParseError> {
        let mut doc = Document::new();
        while !self.peek_punct('}') {
            let at = self
                .toks
                .get(self.pos)
                .map_or(self.src.len(), |t| t.1.start);
            let key = match self.next() {
                Some(Tok::Word(w)) | Some(Tok::Str(w)) | Some(Tok::Number(w)) => w,
                _ => return Err(self.error_at(at, "expected a key")),
            };
            if key.contains('\0') {
                return Err(self.error_at(at, "a key can't hold a NUL"));
            }
            self.expect_punct(':')?;
            let value = self.value()?;
            if doc.contains_key(&key) {
                return Err(self.error_at(at, &format!("the key `{key}` appears twice")));
            }
            doc.insert(key, value);
            if !self.peek_punct(',') {
                break;
            }
            self.pos += 1;
        }
        if !self.peek_punct('}') {
            return Err(if self.pos >= self.toks.len() {
                self.error_at(open, "unclosed `{`")
            } else {
                self.error("expected `,` or `}`")
            });
        }
        self.pos += 1;
        Ok(Bson::Document(doc))
    }

    /// `true`, `null`, `MinKey` and the shell's helpers, such as `ObjectId("...")`.
    fn word_value(&mut self, word: &str, at: usize) -> Result<Bson, ParseError> {
        let constant = match word {
            "true" => Some(Bson::Boolean(true)),
            "false" => Some(Bson::Boolean(false)),
            "null" => Some(Bson::Null),
            "Infinity" => Some(Bson::Double(f64::INFINITY)),
            "NaN" => Some(Bson::Double(f64::NAN)),
            "MinKey" | "MaxKey" => {
                if self.peek_punct('(') {
                    self.no_args()?;
                }
                Some(if word == "MinKey" {
                    Bson::MinKey
                } else {
                    Bson::MaxKey
                })
            }
            _ => None,
        };
        if let Some(value) = constant {
            return Ok(value);
        }
        let (helper, helper_at) = if word == "new" {
            let helper_at = self
                .toks
                .get(self.pos)
                .map_or(self.src.len(), |t| t.1.start);
            (self.word("a type after `new`")?, helper_at)
        } else {
            (word.to_owned(), at)
        };
        if !self.peek_punct('(') {
            return Err(self.error_at(
                helper_at,
                &format!("unknown value `{helper}`: JavaScript isn't evaluated"),
            ));
        }
        let args = self.args()?;
        let fail = |message: &str| self.error_at(helper_at, message);
        let only_string = |args: &[(Bson, usize)]| match args {
            [(Bson::String(s), _)] => Some(s.clone()),
            _ => None,
        };
        match helper.as_str() {
            "ObjectId" => match args.as_slice() {
                [] => Ok(Bson::ObjectId(ObjectId::new())),
                _ => only_string(&args)
                    .and_then(|s| ObjectId::parse_str(s).ok())
                    .map(Bson::ObjectId)
                    .ok_or_else(|| fail("ObjectId takes 24 hexadecimal digits")),
            },
            "ISODate" | "Date" => match args.as_slice() {
                [] => Ok(Bson::DateTime(DateTime::now())),
                [(Bson::Int32(ms), _)] => Ok(Bson::DateTime(DateTime::from_millis((*ms).into()))),
                [(Bson::Int64(ms), _)] => Ok(Bson::DateTime(DateTime::from_millis(*ms))),
                _ => only_string(&args)
                    .and_then(|s| parse_date(&s))
                    .map(Bson::DateTime)
                    .ok_or_else(|| fail("expected a date such as \"2024-01-31T12:00:00Z\"")),
            },
            "NumberLong" | "NumberInt" => {
                let n = match args.as_slice() {
                    [(Bson::Int32(n), _)] => Some(i64::from(*n)),
                    [(Bson::Int64(n), _)] => Some(*n),
                    [(Bson::String(s), _)] => s.trim().parse::<i64>().ok(),
                    _ => None,
                };
                let n = n.ok_or_else(|| fail(&format!("{helper} takes a whole number")))?;
                if helper == "NumberInt" {
                    i32::try_from(n)
                        .map(Bson::Int32)
                        .map_err(|_| fail("NumberInt takes a 32-bit number"))
                } else {
                    Ok(Bson::Int64(n))
                }
            }
            "NumberDecimal" | "Decimal128" => {
                let text = match args.as_slice() {
                    [(Bson::String(s), _)] => Some(s.clone()),
                    [(Bson::Int32(n), _)] => Some(n.to_string()),
                    [(Bson::Int64(n), _)] => Some(n.to_string()),
                    _ => None,
                };
                text.and_then(|s| Decimal128::from_str(s.trim()).ok())
                    .map(Bson::Decimal128)
                    .ok_or_else(|| fail("NumberDecimal takes a number as a string"))
            }
            "UUID" => match args.as_slice() {
                [] => Ok(Bson::Binary(Binary::from_uuid(Uuid::new()))),
                _ => only_string(&args)
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .map(|u| Bson::Binary(Binary::from_uuid(u)))
                    .ok_or_else(|| fail("UUID takes a UUID as a string")),
            },
            "Timestamp" => {
                let u32_of = |b: &Bson| match b {
                    Bson::Int32(n) => u32::try_from(*n).ok(),
                    Bson::Int64(n) => u32::try_from(*n).ok(),
                    _ => None,
                };
                let parts = match args.as_slice() {
                    [(t, _), (i, _)] => u32_of(t).zip(u32_of(i)),
                    [(Bson::Document(d), _)] if d.len() == 2 => {
                        d.get("t").and_then(u32_of).zip(d.get("i").and_then(u32_of))
                    }
                    _ => None,
                };
                parts
                    .map(|(time, increment)| Bson::Timestamp(Timestamp { time, increment }))
                    .ok_or_else(|| fail("Timestamp takes (seconds, increment) or {t, i}"))
            }
            "RegExp" => match args.as_slice() {
                [(Bson::String(p), _)] => regex(p, ""),
                [(Bson::String(p), _), (Bson::String(f), _)]
                    if f.chars().all(|c| "imsxu".contains(c)) =>
                {
                    regex(p, f)
                }
                _ => None,
            }
            .ok_or_else(|| fail("RegExp takes a pattern and flags as strings")),
            other => Err(fail(&format!(
                "unknown function `{other}`: JavaScript isn't evaluated"
            ))),
        }
    }
}

/// The error for the current argument list, at its method.
fn self_error(args: &Args<'_, '_>, message: &str) -> ParseError {
    args.parser.error_at(args.method_at, message)
}

/// The arguments of one method call, read in order.
struct Args<'p, 'a> {
    parser: &'p Parser<'a>,
    args: Vec<(Bson, usize)>,
    next: usize,
    method: &'p str,
    method_at: usize,
}

impl Args<'_, '_> {
    fn take(&mut self) -> Option<(Bson, usize)> {
        let arg = self.args.get(self.next).cloned();
        if arg.is_some() {
            self.next += 1;
        }
        arg
    }

    fn missing(&self, what: &str) -> ParseError {
        self.parser
            .error_at(self.method_at, &format!("{} needs {what}", self.method))
    }

    fn value(&mut self, what: &str) -> Result<Bson, ParseError> {
        self.take()
            .map(|(v, _)| v)
            .ok_or_else(|| self.missing(what))
    }

    fn doc(&mut self, what: &str) -> Result<Document, ParseError> {
        match self.take() {
            Some((Bson::Document(d), _)) => Ok(d),
            Some((_, at)) => Err(self
                .parser
                .error_at(at, &format!("expected the {what} as a document"))),
            None => Err(self.missing(what)),
        }
    }

    fn optional_doc(&mut self, what: &str) -> Result<Option<Document>, ParseError> {
        match self.args.get(self.next) {
            None => Ok(None),
            Some(_) => self.doc(what).map(Some),
        }
    }

    fn docs(&mut self, what: &str) -> Result<Vec<Document>, ParseError> {
        match self.take() {
            Some((Bson::Array(items), at)) => items
                .into_iter()
                .map(|item| match item {
                    Bson::Document(d) => Ok(d),
                    _ => Err(self
                        .parser
                        .error_at(at, &format!("each item of the {what} must be a document"))),
                })
                .collect(),
            Some((_, at)) => Err(self
                .parser
                .error_at(at, &format!("expected the {what} as an array"))),
            None => Err(self.missing(what)),
        }
    }

    fn string(&mut self, what: &str) -> Result<String, ParseError> {
        match self.take() {
            Some((Bson::String(s), _)) => Ok(s),
            Some((_, at)) => Err(self
                .parser
                .error_at(at, &format!("expected the {what} as a string"))),
            None => Err(self.missing(what)),
        }
    }

    fn integer(&mut self, what: &str) -> Result<i64, ParseError> {
        match self.take() {
            Some((Bson::Int32(n), _)) => Ok(n.into()),
            Some((Bson::Int64(n), _)) => Ok(n),
            Some((_, at)) => Err(self
                .parser
                .error_at(at, &format!("expected the {what} as a whole number"))),
            None => Err(self.missing(what)),
        }
    }

    /// The options of an update or replace: only `upsert`.
    fn upsert(&mut self) -> Result<Option<bool>, ParseError> {
        let Some(options) = self.optional_doc("options")? else {
            return Ok(None);
        };
        let mut upsert = None;
        for (key, value) in options {
            match (key.as_str(), value) {
                ("upsert", Bson::Boolean(b)) => upsert = Some(b),
                (key, _) => {
                    return Err(self.parser.error_at(
                        self.method_at,
                        &format!("unsupported option `{key}`: only `upsert` is"),
                    ));
                }
            }
        }
        Ok(upsert)
    }

    fn done(&self) -> Result<(), ParseError> {
        match self.args.get(self.next) {
            None => Ok(()),
            Some((_, at)) => Err(self
                .parser
                .error_at(*at, &format!("too many arguments for {}", self.method))),
        }
    }
}

fn aggregate_options(doc: Document) -> Result<AggregateOptions, String> {
    let mut options = AggregateOptions::default();
    let whole = |v: &Bson| match v {
        Bson::Int32(n) => u64::try_from(*n).ok(),
        Bson::Int64(n) => u64::try_from(*n).ok(),
        _ => None,
    };
    for (key, value) in doc {
        match key.as_str() {
            "allowDiskUse" => match value {
                Bson::Boolean(b) => options.allow_disk_use = Some(b),
                _ => return Err("allowDiskUse takes true or false".into()),
            },
            "maxTimeMS" => {
                options.max_time_ms = Some(whole(&value).ok_or("maxTimeMS takes a whole number")?);
            }
            "batchSize" => {
                let n = whole(&value).and_then(|n| u32::try_from(n).ok());
                options.batch_size = Some(n.ok_or("batchSize takes a whole number")?);
            }
            other => {
                return Err(format!(
                    "unsupported aggregate option `{other}`: allowDiskUse, maxTimeMS and \
                     batchSize are"
                ));
            }
        }
    }
    Ok(options)
}

/// A whole number that fits 32 bits is an `int`, as the shell sends it; a larger one a
/// `long`; anything else a `double`.
fn number(text: &str, negative: bool) -> Option<Bson> {
    let signed = if negative {
        format!("-{text}")
    } else {
        text.to_owned()
    };
    if !text.contains(['.', 'e', 'E']) {
        if let Ok(n) = signed.parse::<i32>() {
            return Some(Bson::Int32(n));
        }
        if let Ok(n) = signed.parse::<i64>() {
            return Some(Bson::Int64(n));
        }
    }
    signed.parse::<f64>().ok().map(Bson::Double)
}

fn regex(pattern: &str, flags: &str) -> Option<Bson> {
    let mut flags: Vec<char> = flags.chars().collect();
    flags.sort_unstable();
    flags.dedup();
    Some(Bson::RegularExpression(Regex {
        pattern: CString::try_from(pattern).ok()?,
        options: CString::try_from(flags.into_iter().collect::<String>()).ok()?,
    }))
}

/// An RFC 3339 date, or a plain `YYYY-MM-DD`, which the shell reads as midnight UTC.
fn parse_date(text: &str) -> Option<DateTime> {
    let text = text.trim();
    DateTime::parse_rfc3339_str(text)
        .ok()
        .or_else(|| DateTime::parse_rfc3339_str(format!("{text}T00:00:00Z")).ok())
        .or_else(|| DateTime::parse_rfc3339_str(format!("{text}Z")).ok())
}

#[cfg(test)]
mod tests {
    use mongodb::bson::doc;

    use super::*;

    fn parse(src: &str) -> Command {
        parse_command(src).unwrap_or_else(|e| panic!("{src}: {e}"))
    }

    fn op(src: &str) -> Op {
        match parse(src) {
            Command::Collection { op, .. } => op,
            other => panic!("{src}: {other:?}"),
        }
    }

    fn err(src: &str) -> ParseError {
        parse_command(src).expect_err(src)
    }

    fn users() -> Target {
        Target {
            database: None,
            collection: "users".into(),
        }
    }

    /// The method lists the editor offers stay those the parser reads.
    #[test]
    fn every_listed_method_parses() {
        let args = |method: &str| match method {
            "aggregate" | "insertMany" => "[{}]",
            "distinct" => "'a'",
            "insertOne" | "deleteOne" | "deleteMany" | "createIndex" => "{a: 1}",
            "updateOne" | "updateMany" | "replaceOne" => "{}, {a: 1}",
            "dropIndex" => "'a_1'",
            _ => "",
        };
        for method in READ_METHODS.iter().chain(&WRITE_METHODS) {
            let src = format!("db.c.{method}({})", args(method));
            assert!(parse_command(&src).is_ok(), "{src}");
        }
        for method in FIND_CURSOR_METHODS {
            let arg = match method {
                "sort" | "projection" => "{a: 1}",
                "limit" | "skip" => "1",
                _ => "",
            };
            let src = format!("db.c.find().{method}({arg})");
            assert!(parse_command(&src).is_ok(), "{src}");
        }
        for method in AGGREGATE_CURSOR_METHODS {
            let src = format!("db.c.aggregate([]).{method}()");
            assert!(parse_command(&src).is_ok(), "{src}");
        }
        for (method, arg) in DATABASE_METHODS.iter().zip([
            "'c').find(",
            "'d').c.find(",
            "",
            "{ping: 1}",
            "{ping: 1}",
        ]) {
            let src = format!("db.{method}({arg})");
            assert!(parse_command(&src).is_ok(), "{src}");
        }
    }

    #[test]
    fn find_with_a_chain_and_relaxed_json() {
        let cmd = parse(
            "db.users.find({ name: 'Ada', age: {$gte: 30,}, // note\n 'a b': \"x\" }, {name: 1})\n  .sort({age: -1}) /* c */ .limit(10).skip(2);",
        );
        assert_eq!(
            cmd,
            Command::Collection {
                target: users(),
                op: Op::Find {
                    filter: doc! {"name": "Ada", "age": {"$gte": 30}, "a b": "x"},
                    projection: Some(doc! {"name": 1}),
                    sort: Some(doc! {"age": -1}),
                    limit: Some(10),
                    skip: Some(2),
                }
            }
        );
    }

    #[test]
    fn targets_by_name_by_get_collection_and_in_another_database() {
        assert_eq!(
            parse("db.getCollection(\"my coll\").countDocuments()"),
            Command::Collection {
                target: Target {
                    database: None,
                    collection: "my coll".into()
                },
                op: Op::CountDocuments {
                    filter: Document::new()
                }
            }
        );
        assert_eq!(
            parse("db.getSiblingDB('shop').orders.estimatedDocumentCount()"),
            Command::Collection {
                target: Target {
                    database: Some("shop".into()),
                    collection: "orders".into()
                },
                op: Op::EstimatedDocumentCount
            }
        );
        assert_eq!(
            parse("db.system.views.find()"),
            Command::Collection {
                target: Target {
                    database: None,
                    collection: "system.views".into()
                },
                op: Op::Find {
                    filter: Document::new(),
                    projection: None,
                    sort: None,
                    limit: None,
                    skip: None
                }
            }
        );
        assert_eq!(
            parse("db.getSiblingDB(\"a\").getSiblingDB(\"b\").getCollectionNames()"),
            Command::ShowCollections {
                database: Some("b".into())
            }
        );
    }

    #[test]
    fn shell_commands_and_run_command() {
        assert_eq!(parse("show dbs"), Command::ShowDatabases);
        assert_eq!(
            parse("show collections;"),
            Command::ShowCollections { database: None }
        );
        assert_eq!(
            parse("db.runCommand({ping: 1})"),
            Command::RunCommand {
                database: None,
                command: doc! {"ping": 1}
            }
        );
        assert_eq!(
            parse("db.adminCommand({listDatabases: 1})"),
            Command::RunCommand {
                database: Some("admin".into()),
                command: doc! {"listDatabases": 1}
            }
        );
    }

    #[test]
    fn helpers_make_bson_values() {
        let Op::InsertOne { document } = op("db.users.insertOne({\
             id: ObjectId('65a1b2c3d4e5f60718293a4b'), \
             at: ISODate('2024-01-02T03:04:05Z'), day: new Date('2024-01-02'), \
             l: NumberLong('9007199254740993'), i: NumberInt(7), d: NumberDecimal('1.10'), \
             u: UUID('0e2bf3c4-3c39-4bd6-8f0b-0a7a3b0e8b51'), t: Timestamp(5, 1), \
             lo: MinKey, hi: MaxKey(), re: /^a\\/b[/]c/i, big: 3000000000, f: 1.5e3, n: -2, \
             s: \"\\u0041\\x42\\n\" })")
        else {
            panic!()
        };
        assert_eq!(
            document.get("id"),
            Some(&Bson::ObjectId(
                ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap()
            ))
        );
        assert_eq!(
            document.get("at"),
            Some(&Bson::DateTime(DateTime::from_millis(1704164645000)))
        );
        assert_eq!(
            document.get("day"),
            Some(&Bson::DateTime(DateTime::from_millis(1704153600000)))
        );
        assert_eq!(document.get("l"), Some(&Bson::Int64(9007199254740993)));
        assert_eq!(document.get("i"), Some(&Bson::Int32(7)));
        assert!(matches!(document.get("d"), Some(Bson::Decimal128(_))));
        assert!(matches!(document.get("u"), Some(Bson::Binary(b)) if b.bytes.len() == 16));
        assert_eq!(
            document.get("t"),
            Some(&Bson::Timestamp(Timestamp {
                time: 5,
                increment: 1
            }))
        );
        assert_eq!(document.get("lo"), Some(&Bson::MinKey));
        assert_eq!(document.get("hi"), Some(&Bson::MaxKey));
        let Some(Bson::RegularExpression(re)) = document.get("re") else {
            panic!()
        };
        assert_eq!(
            (re.pattern.as_str(), re.options.as_str()),
            ("^a\\/b[/]c", "i")
        );
        assert_eq!(document.get("big"), Some(&Bson::Int64(3000000000)));
        assert_eq!(document.get("f"), Some(&Bson::Double(1500.0)));
        assert_eq!(document.get("n"), Some(&Bson::Int32(-2)));
        assert_eq!(document.get_str("s").unwrap(), "AB\n");
    }

    #[test]
    fn escapes_are_decoded_in_keys_too() {
        let Op::Aggregate { pipeline, .. } = op(r#"db.users.aggregate([{"$out": "x"}])"#) else {
            panic!()
        };
        assert_eq!(pipeline, [doc! {"$out": "x"}]);
        let Op::Find { filter, .. } = op(r#"db.users.find({'\x24where': 1})"#) else {
            panic!()
        };
        assert!(filter.contains_key("$where"));
    }

    #[test]
    fn writes_and_their_options() {
        assert_eq!(
            op("db.users.updateMany({a: 1}, {$set: {b: 2}}, {upsert: true})"),
            Op::Update {
                many: true,
                filter: doc! {"a": 1},
                update: Bson::Document(doc! {"$set": {"b": 2}}),
                upsert: Some(true)
            }
        );
        assert_eq!(
            op("db.users.deleteOne({_id: 1})"),
            Op::Delete {
                many: false,
                filter: doc! {"_id": 1}
            }
        );
        assert_eq!(
            op("db.users.createIndex({email: 1}, {unique: true})"),
            Op::CreateIndex {
                keys: doc! {"email": 1},
                options: Some(doc! {"unique": true})
            }
        );
        assert_eq!(op("db.users.drop()"), Op::Drop);
        assert!(matches!(
            op("db.users.find().explain()"),
            Op::Explain(inner) if matches!(*inner, Op::Find { .. })
        ));
        assert_eq!(
            op("db.users.aggregate([{$match: {}}], {allowDiskUse: true, maxTimeMS: 50})"),
            Op::Aggregate {
                pipeline: vec![doc! {"$match": {}}],
                options: AggregateOptions {
                    allow_disk_use: Some(true),
                    max_time_ms: Some(50),
                    batch_size: None
                }
            }
        );
    }

    #[test]
    fn errors_say_where() {
        let e = err("db.users.find({a: 1,, })");
        assert_eq!((e.line, e.column), (1, 21), "{e}");
        let e = err("db.users\n  .find({a: function() { return 1 }})");
        assert_eq!(e.line, 2, "{e}");
        assert!(e.message.contains("JavaScript"), "{e}");
        let e = err("db.users.find({a: 1, a: 2})");
        assert!(e.message.contains("twice"), "{e}");
        assert_eq!(e.column, 22);
        for (src, needle) in [
            ("db.users.find({a: 'x})", "unterminated string"),
            ("db.users.find({a: 1}", "expected `)`"),
            ("db.users.mapReduce(1, 2)", "unsupported collection method"),
            ("db.users.find().forEach(printjson)", "cursor method"),
            ("use shop", "getSiblingDB"),
            ("db.users.find({}) db.users.drop()", "end of the command"),
            ("db.users.find({}); db.users.drop()", "end of the command"),
            ("print('x')", "expected a command"),
            ("db.users.find({a: `x${y}`})", "template"),
            ("db.users.find({a: x})", "JavaScript"),
            (
                "db.users.aggregate([], {out: 'x'})",
                "unsupported aggregate option",
            ),
            (
                "db.users.updateOne({}, {$set: {a: 1}}, {writeConcern: {w: 0}})",
                "only `upsert`",
            ),
            ("db.users.find({a: /x/g})", "flag"),
            ("db.users.find({} /* open", "unterminated comment"),
            ("db.users.find().limit(1.5)", "whole number"),
            ("db.foo()", "unknown database method"),
        ] {
            let e = err(src);
            assert!(e.message.contains(needle), "{src}: {e}");
        }
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_crash() {
        let src = format!("db.c.find({}{})", "{a:".repeat(500), "}".repeat(500));
        assert!(err(&src).message.contains("deeply"));
    }

    #[test]
    fn as_read_keeps_reads_and_drops_writes() {
        let read = |src: &str| parse(src).as_read();
        assert!(matches!(read("db.users.find()"), Some(ReadOp::Find { .. })));
        assert_eq!(read("show dbs"), Some(ReadOp::ListDatabases));
        assert!(matches!(
            read("db.users.find().explain()"),
            Some(ReadOp::Explain(_))
        ));
        for src in [
            "db.users.insertOne({})",
            "db.users.updateOne({}, {$set: {a: 1}})",
            "db.users.drop()",
            "db.users.createIndex({a: 1})",
            "db.runCommand({ping: 1})",
            "db.users.aggregate([{$out: 'x'}])",
            "db.users.aggregate([{$facet: {a: [{$merge: {into: 'x'}}]}}])",
            "db.users.aggregate([{$lookup: {from: 'o', pipeline: [{$out: 'x'}], as: 'o'}}])",
            "db.users.find({$where: 'sleep(1)'})",
            "db.users.find({}, {a: {$function: {}}})",
            "db.users.find().sort({a: {$accumulator: 1}})",
            "db.users.countDocuments({$where: '1'})",
            "db.users.distinct('a', {$where: '1'})",
            "db.$cmd.findOne({drop: 'users'})",
        ] {
            assert_eq!(read(src), None, "{src}");
        }
    }

    fn pieces(src: &str) -> Vec<&str> {
        split(src).into_iter().map(|p| &src[p.text]).collect()
    }

    #[test]
    fn splits_at_semicolons_and_line_breaks_but_keeps_chains() {
        assert_eq!(
            pieces("db.a.find()\ndb.b.find();db.c.find()  ;\n\n"),
            ["db.a.find()", "db.b.find()", "db.c.find()"]
        );
        assert_eq!(
            pieces("db.a.find({\n  x: 1\n})\n  .sort({x: 1})\n  // c\n  .limit(5)\ndb.b.drop()"),
            [
                "db.a.find({\n  x: 1\n})\n  .sort({x: 1})\n  // c\n  .limit(5)",
                "db.b.drop()"
            ]
        );
        assert_eq!(pieces("db.a.\nfind()"), ["db.a.\nfind()"]);
        assert_eq!(
            pieces("// only a comment\n/* and another */"),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn nothing_quoted_or_commented_hides_a_statement() {
        // Each holds what looks like a second statement inside a string, a comment or
        // a regular expression: one piece, and it parses as one read.
        for src in [
            "db.a.find({x: 'a; db.a.drop()'})",
            "db.a.find({x: \"a\\\"; db.a.drop()\"})",
            "db.a.find({x: `a\ndb.a.drop()`})",
            "db.a.find({x: /a;\\/db.a.drop()/})",
            "db.a.find({x: /[/;]/}) // ; db.a.drop()",
            "db.a.find({x: 1 /* ;\ndb.a.drop() */})",
        ] {
            assert_eq!(pieces(src).len(), 1, "{src}");
            assert!(parse(src).as_read().is_some(), "{src}");
        }
        // The other way round: a comment that ends early leaves the write in the code,
        // where it is a statement of its own.
        let src = "db.a.find() /* x */ ; db.a.drop()";
        assert_eq!(pieces(src), ["db.a.find() /* x */", "db.a.drop()"]);
        // A line comment ends at the line break, so what follows is code.
        assert_eq!(
            pieces("db.a.find() // x\ndb.a.drop()"),
            ["db.a.find() // x", "db.a.drop()"]
        );
        // An unterminated string runs to the end, and doesn't parse.
        let src = "db.a.find({x: 'a})\ndb.a.drop()";
        assert_eq!(pieces(src).len(), 1);
        assert!(parse_command(src).is_err());
    }

    #[test]
    fn a_script_parses_statement_by_statement_with_positions_in_the_script() {
        let cmds = parse_script("db.a.find()\nshow dbs").unwrap();
        assert_eq!(cmds.len(), 2);
        let e = parse_script("db.a.find()\ndb.b.find({,})").unwrap_err();
        assert_eq!((e.line, e.column), (2, 12), "{e}");
    }

    #[test]
    fn division_is_not_a_regex_outside_a_value() {
        // `/` after a value is not a regex start, so this is a stray `/`.
        let e = err("db.a.find({a: 1 / 2})");
        assert!(e.message.contains("expected `,` or `}`"), "{e}");
        assert_eq!(e.column, 17);
    }
}
