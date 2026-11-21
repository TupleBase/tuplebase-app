//! Autocomplete in the query editor. SQL gets keywords plus the connection's table and
//! column names; Mongo shell commands get collections, methods, `$` operators and stages,
//! and sampled field names ([`MongoCompletions`]).
//!
//! The menu opens by itself once a word has [`MIN_TYPED`] characters, or at once with
//! Ctrl+Space. Tab takes the highlighted suggestion; Enter always starts a new line, and
//! Escape closes the menu.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::Result;
use gpui_kit::component::input::RopeExt as _;
use gpui_kit::component::input::{CompletionProvider, EditorState, Enter, ShowCompletions};
use gpui_kit::{
    App, Context, Div, Entity, InteractiveElement as _, KeyBinding, Stateful, Task, Window, actions,
};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    CompletionTriggerKind, TextEdit,
};
use ropey::Rope;
use tuplebase_core::config::Adapter;
use tuplebase_core::{mongo_shell, safety};

const MAX_SUGGESTIONS: usize = 50;

/// Characters of a word typed before the menu opens by itself, so it doesn't pop up at
/// every letter.
const MIN_TYPED: usize = 2;

/// Key context of a code editor, so other inputs keep their own Tab and Enter.
const EDITOR_CONTEXT: &str = "Input && mode == editor";

actions!(completions, [AcceptCompletion, NewLine]);

/// Call after `gpui_kit::init`: these override the editor's own bindings, where Enter
/// takes the suggestion and Tab only indents.
pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("tab", AcceptCompletion, Some(EDITOR_CONTEXT)),
        KeyBinding::new("enter", NewLine, Some(EDITOR_CONTEXT)),
        KeyBinding::new("shift-enter", NewLine, Some(EDITOR_CONTEXT)),
        KeyBinding::new("ctrl-space", ShowCompletions, Some(EDITOR_CONTEXT)),
    ]
}

/// Handles [`key_bindings`] for `editor`, placed on an element around it.
pub fn handle_keys(
    element: Stateful<Div>,
    editor: &Entity<EditorState>,
    window: &Window,
) -> Stateful<Div> {
    element
        .on_action(window.listener_for(editor, accept))
        .on_action(window.listener_for(editor, new_line))
}

/// Takes the highlighted suggestion when the menu is open, and indents otherwise.
fn accept(
    editor: &mut EditorState,
    _: &AcceptCompletion,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    // The menu takes a suggestion on Enter; it isn't told which key was pressed.
    let enter = Enter {
        secondary: false,
        shift: false,
    };
    if editor.route_overlay_action(Box::new(enter), window, cx) {
        // The menu lets the action go on; stop it before the next binding, Tab's indent.
        cx.stop_propagation();
    } else {
        cx.propagate();
    }
}

/// Closes the menu, so that the editor's own Enter, next, starts a new line.
fn new_line(editor: &mut EditorState, _: &NewLine, _: &mut Window, cx: &mut Context<EditorState>) {
    editor.dismiss_completion_overlay(cx);
    cx.propagate();
}

const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "AND",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "COMMIT",
    "COUNT",
    "CREATE",
    "CROSS",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXISTS",
    "EXPLAIN",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTO",
    "IS",
    "JOIN",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "SELECT",
    "SET",
    "SHOW",
    "TABLE",
    "THEN",
    "TRANSACTION",
    "UNION",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WITH",
];

/// The names of the current connection that completions offer, updated by the schema
/// panel.
#[derive(Default)]
pub struct Names {
    /// Each table or collection, with the schema or database it is in.
    tables: Vec<(String, String)>,
    /// Column or field names, sorted, without repeats.
    columns: Vec<String>,
    /// Table and column names together, sorted, without repeats: what SQL offers.
    all: Vec<String>,
}

impl Names {
    /// Takes these tables and columns in place of the ones known so far.
    pub fn set(&mut self, tables: Vec<(String, String)>, columns: Vec<String>) {
        self.tables = tables;
        self.columns = columns;
        self.columns.sort();
        self.columns.dedup();
        self.update_all();
    }

    /// Adds column names, such as the fields a collection's sample found.
    pub fn add_columns(&mut self, columns: impl IntoIterator<Item = String>) {
        self.columns.extend(columns);
        self.columns.sort();
        self.columns.dedup();
        self.update_all();
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    fn update_all(&mut self) {
        let mut all: Vec<String> = self.tables.iter().map(|(_, name)| name.clone()).collect();
        all.extend(self.columns.iter().cloned());
        all.sort();
        all.dedup();
        self.all = all;
    }

    /// The tables or collections of one schema or database.
    fn tables_in<'a>(&'a self, schema: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.tables
            .iter()
            .filter(move |(s, _)| s == schema)
            .map(|(_, name)| name.as_str())
    }
}

/// The current connection's names, shared by the schema panel and the completions.
pub type SchemaNames = Rc<RefCell<Names>>;

pub struct SqlCompletions {
    names: SchemaNames,
    /// Where the word started when the menu was asked for with Ctrl+Space: while that
    /// word is typed, the menu stays open even under [`MIN_TYPED`] characters.
    invoked_at: Cell<Option<usize>>,
}

impl SqlCompletions {
    pub fn new(names: SchemaNames) -> Self {
        Self {
            names,
            invoked_at: Cell::new(None),
        }
    }

    /// The fewest characters the word at the cursor needs for a menu.
    fn min_typed(&self, word_start: usize, trigger: &CompletionContext) -> usize {
        if trigger.trigger_kind == CompletionTriggerKind::INVOKED {
            self.invoked_at.set(Some(word_start));
        } else if self.invoked_at.get() != Some(word_start) {
            self.invoked_at.set(None);
        }
        if self.invoked_at.get().is_some() {
            0
        } else {
            MIN_TYPED
        }
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The partly typed word that ends at the cursor.
fn word_before(text: &str) -> &str {
    let start = text
        .char_indices()
        .rev()
        .find(|(_, c)| !is_word_char(*c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    &text[start..]
}

/// Suggestions for `prefix`: names from the database first, then keywords. An empty
/// prefix gets the first of all of them.
fn suggestions(prefix: &str, names: &[String]) -> Vec<(String, CompletionItemKind)> {
    let prefix = prefix.to_lowercase();
    let starts = |candidate: &str| candidate.to_lowercase().starts_with(&prefix);
    names
        .iter()
        .filter(|name| starts(name))
        .map(|name| (name.clone(), CompletionItemKind::FIELD))
        .chain(
            KEYWORDS
                .iter()
                .filter(|keyword| starts(keyword))
                .map(|keyword| ((*keyword).to_owned(), CompletionItemKind::KEYWORD)),
        )
        .take(MAX_SUGGESTIONS)
        .collect()
}

impl CompletionProvider for SqlCompletions {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let text = text.to_string();
        let before = &text[..offset.min(text.len())];
        let prefix = word_before(before);
        let min_typed = self.min_typed(before.len() - prefix.len(), &trigger);
        if prefix.chars().count() < min_typed {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        }
        let items = suggestions(prefix, &self.names.borrow().all)
            .into_iter()
            .map(|(label, kind)| CompletionItem {
                label,
                kind: Some(kind),
                ..Default::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        new_text.chars().next_back().is_some_and(is_word_char)
    }
}

/// `$` operators offered in filters, projections and expressions. The ones that run
/// JavaScript on the server (`$where`, `$function`, `$accumulator`) are never offered.
const MONGO_OPERATORS: &[&str] = &[
    "$eq",
    "$ne",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$in",
    "$nin",
    "$and",
    "$or",
    "$nor",
    "$not",
    "$exists",
    "$type",
    "$regex",
    "$options",
    "$elemMatch",
    "$size",
    "$all",
    "$expr",
    "$mod",
    "$text",
    "$search",
    "$jsonSchema",
    "$geoWithin",
    "$geoIntersects",
    "$near",
    "$nearSphere",
    "$comment",
    "$sum",
    "$avg",
    "$min",
    "$max",
    "$first",
    "$last",
    "$push",
    "$addToSet",
    "$count",
    "$cond",
    "$ifNull",
    "$switch",
    "$concat",
    "$toUpper",
    "$toLower",
    "$substrCP",
    "$split",
    "$toString",
    "$toInt",
    "$toDate",
    "$dateToString",
    "$year",
    "$month",
    "$dayOfMonth",
    "$arrayElemAt",
    "$filter",
    "$map",
    "$reduce",
    "$add",
    "$subtract",
    "$multiply",
    "$divide",
    "$round",
    "$literal",
    "$mergeObjects",
    "$objectToArray",
    "$let",
];

/// `$` operators of an update, offered only on a writable connection.
const MONGO_UPDATE_OPERATORS: &[&str] = &[
    "$set",
    "$unset",
    "$inc",
    "$mul",
    "$rename",
    "$setOnInsert",
    "$currentDate",
    "$pull",
    "$pullAll",
    "$pop",
    "$each",
    "$position",
];

/// The stages that write, offered only on a writable connection.
const MONGO_WRITE_STAGES: &[&str] = &["$out", "$merge"];

/// Shell helpers for values, offered with the field names.
const MONGO_HELPERS: &[&str] = &[
    "ObjectId",
    "ISODate",
    "NumberLong",
    "NumberInt",
    "NumberDecimal",
    "UUID",
    "Timestamp",
];

/// What the connection allows, which narrows what is offered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MongoScope {
    /// The database `db` means: the connection's, or `test`.
    pub database: String,
    /// Read-only connections are offered only what the read-only guard lets through.
    pub read_only: bool,
}

/// Completions for Mongo shell commands: collection names after `db.`, methods after
/// `db.<collection>.`, `$` operators and aggregation stages after `$`, and else the
/// sampled field names.
pub struct MongoCompletions {
    names: SchemaNames,
    scope: MongoScope,
    invoked_at: Cell<Option<usize>>,
}

impl MongoCompletions {
    pub fn new(names: SchemaNames, scope: MongoScope) -> Self {
        Self {
            names,
            scope,
            invoked_at: Cell::new(None),
        }
    }
}

/// Where the cursor is in a Mongo shell command, which decides what is offered.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MongoContext {
    /// After `db.`, or `db.getSiblingDB("name").`: collections of that database.
    Database(Option<String>),
    /// After `db.<collection>.`.
    Collection,
    /// After `find(...).` and its chain.
    FindCursor,
    /// After `aggregate(...).`.
    AggregateCursor,
    /// A `$` word where a pipeline stage goes.
    Stage,
    /// Any other `$` word.
    Operator,
    /// Anything else: field names and helpers.
    Other,
    /// Inside a string or a comment, or after a `.` that follows nothing known.
    Nothing,
}

/// One segment of a dotted chain such as `db.users.find({})`.
#[derive(Debug, PartialEq)]
enum Segment<'a> {
    Name(&'a str),
    /// A call, with the text between its parentheses.
    Call(&'a str, &'a str),
}

fn is_ident_char(c: char) -> bool {
    is_word_char(c) || c == '$'
}

/// The identifier that ends `text`, if any.
fn ident_before(text: &str) -> &str {
    let start = text
        .char_indices()
        .rev()
        .find(|(_, c)| !is_ident_char(*c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    &text[start..]
}

/// The dotted chain that ends `text` (which ends just before a `.`), first segment first.
/// Brackets in strings may confuse it; the cost is a menu that doesn't open.
fn chain_before(text: &str) -> Option<Vec<Segment<'_>>> {
    let mut segments = Vec::new();
    let mut end = text.trim_end().len();
    loop {
        let head = &text[..end];
        if head.ends_with(')') {
            let mut depth = 0usize;
            let mut open = None;
            for (i, c) in head.char_indices().rev() {
                match c {
                    ')' | ']' | '}' => depth += 1,
                    '(' | '[' | '{' => {
                        depth -= 1;
                        if depth == 0 {
                            open = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let open = open?;
            let args = &head[open + 1..head.len() - 1];
            let before = head[..open].trim_end();
            let name = ident_before(before);
            if name.is_empty() {
                return None;
            }
            segments.push(Segment::Call(name, args));
            end = before.len() - name.len();
        } else {
            let name = ident_before(head);
            if name.is_empty() {
                return None;
            }
            segments.push(Segment::Name(name));
            end = head.len() - name.len();
        }
        let rest = text[..end].trim_end();
        match rest.strip_suffix('.') {
            Some(rest) => end = rest.trim_end().len(),
            None => break,
        }
    }
    segments.reverse();
    Some(segments)
}

/// The string a call such as `getSiblingDB("app")` was given.
fn string_arg(args: &str) -> Option<String> {
    let args = args.trim();
    let quote = args
        .chars()
        .next()
        .filter(|c| matches!(c, '"' | '\'' | '`'))?;
    let inner = args[1..].strip_suffix(quote)?;
    Some(inner.to_owned())
}

/// What follows the chain before a `.`.
fn chain_context(segments: &[Segment]) -> MongoContext {
    let [Segment::Name("db"), rest @ ..] = segments else {
        return MongoContext::Nothing;
    };
    let (database, rest) = match rest {
        [Segment::Call("getSiblingDB", args), rest @ ..] => (string_arg(args), rest),
        _ => (None, rest),
    };
    if rest.is_empty() {
        return match (segments.len(), database) {
            (1, _) => MongoContext::Database(None),
            (_, Some(name)) => MongoContext::Database(Some(name)),
            (_, None) => MongoContext::Nothing,
        };
    }
    // The collection: `getCollection("c")`, or a name that may hold dots.
    let after = match rest {
        [Segment::Call("getCollection", _), after @ ..] => after,
        _ => {
            let names = rest
                .iter()
                .take_while(|s| matches!(s, Segment::Name(_)))
                .count();
            if names == 0 {
                return MongoContext::Nothing;
            }
            &rest[names..]
        }
    };
    let Some((last, before)) = after.split_last() else {
        return MongoContext::Collection;
    };
    let is_find = |s: &Segment| matches!(s, Segment::Call("find", _));
    match last {
        Segment::Call("aggregate", _) if before.is_empty() => MongoContext::AggregateCursor,
        Segment::Call("find", _) if before.is_empty() => MongoContext::FindCursor,
        Segment::Call(method, _)
            if before.first().is_some_and(is_find)
                && mongo_shell::FIND_CURSOR_METHODS.contains(method)
                && *method != "explain" =>
        {
            MongoContext::FindCursor
        }
        _ => MongoContext::Nothing,
    }
}

/// An open bracket, with the name before it: a call's method, or the key whose value
/// it is.
struct Open {
    bracket: char,
    label: String,
}

/// Reads `text`, a statement up to the cursor, and says where the cursor is.
fn mongo_context(text: &str) -> MongoContext {
    let mut stack: Vec<Open> = Vec::new();
    // The last word, quoted key or `$` word, and the last character that isn't blank.
    let mut word = String::new();
    let mut last_key = String::new();
    let mut last = ' ';
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' | '\'' | '`' => {
                let mut closed = false;
                let mut content = String::new();
                while let Some((_, d)) = chars.next() {
                    if d == '\\' {
                        chars.next();
                    } else if d == c {
                        closed = true;
                        break;
                    } else {
                        content.push(d);
                    }
                }
                if !closed {
                    return MongoContext::Nothing;
                }
                word = content;
                last = c;
                continue;
            }
            '/' if text[i + 1..].starts_with('/') => {
                match text[i..].find('\n') {
                    Some(end) => {
                        while chars.peek().is_some_and(|(j, _)| *j < i + end) {
                            chars.next();
                        }
                    }
                    None => return MongoContext::Nothing,
                }
                continue;
            }
            '/' if text[i + 1..].starts_with('*') => {
                match text[i + 2..].find("*/") {
                    Some(end) => {
                        while chars.peek().is_some_and(|(j, _)| *j < i + 2 + end + 2) {
                            chars.next();
                        }
                    }
                    None => return MongoContext::Nothing,
                }
                continue;
            }
            '(' => stack.push(Open {
                bracket: c,
                label: word.clone(),
            }),
            '[' | '{' => {
                let label = match (last, stack.last()) {
                    (':', _) => last_key.clone(),
                    ('(' | ',', Some(open)) if open.bracket == '(' => open.label.clone(),
                    _ => String::new(),
                };
                stack.push(Open { bracket: c, label });
            }
            ')' | ']' | '}' => {
                stack.pop();
            }
            ':' => last_key = word.clone(),
            _ if is_ident_char(c) => {
                if !is_ident_char(last) || last == '"' {
                    word.clear();
                }
                word.push(c);
            }
            _ => {}
        }
        if !c.is_whitespace() {
            last = c;
        }
    }
    let prefix = ident_before(text);
    let before = &text[..text.len() - prefix.len()];
    if let Some(rest) = prefix.strip_prefix('$') {
        if rest.contains('$') {
            return MongoContext::Nothing;
        }
        let key_position = matches!(before.trim_end().chars().next_back(), Some('{' | ','));
        let n = stack.len();
        let is_stage = key_position
            && n >= 2
            && stack[n - 1].bracket == '{'
            && stack[n - 2].bracket == '['
            && (stack[n - 2].label == "pipeline"
                || stack[n - 2].label == "$facet"
                || (n >= 3
                    && ((stack[n - 3].bracket == '(' && stack[n - 3].label == "aggregate")
                        || (stack[n - 3].bracket == '{' && stack[n - 3].label == "$facet"))));
        return if is_stage {
            MongoContext::Stage
        } else {
            MongoContext::Operator
        };
    }
    if prefix.contains('$') {
        return MongoContext::Nothing;
    }
    match before.trim_end().strip_suffix('.') {
        Some(head) => chain_before(head).map_or(MongoContext::Nothing, |s| chain_context(&s)),
        None => MongoContext::Other,
    }
}

/// Whether a collection's name can follow `db.` as it is.
fn is_plain_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
}

/// The suggestions for `prefix` in `context`: each label, what it inserts and its kind.
fn mongo_suggestions(
    context: &MongoContext,
    prefix: &str,
    names: &Names,
    scope: &MongoScope,
) -> Vec<(String, String, CompletionItemKind)> {
    let lower = prefix.to_lowercase();
    let starts = |candidate: &str| candidate.to_lowercase().starts_with(&lower);
    let mut out: Vec<(String, String, CompletionItemKind)> = Vec::new();
    let mut add = |label: &str, insert: String, kind| {
        if starts(label) && !out.iter().any(|(l, _, _)| l == label) {
            out.push((label.to_owned(), insert, kind));
        }
    };
    let words = |list: &[&str]| list.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
    match context {
        MongoContext::Database(database) => {
            let database = database.as_deref().unwrap_or(&scope.database);
            for name in names.tables_in(database) {
                let insert = if is_plain_name(name) {
                    name.to_owned()
                } else {
                    format!("getCollection({})", serde_json::Value::from(name))
                };
                add(name, insert, CompletionItemKind::CLASS);
            }
            for method in mongo_shell::DATABASE_METHODS {
                let refused = matches!(method, "runCommand" | "adminCommand");
                if !(scope.read_only && refused) {
                    add(method, method.to_owned(), CompletionItemKind::METHOD);
                }
            }
        }
        MongoContext::Collection => {
            let mut methods = words(&mongo_shell::READ_METHODS);
            if !scope.read_only {
                methods.extend(words(&mongo_shell::WRITE_METHODS));
            }
            for method in methods {
                add(&method, method.clone(), CompletionItemKind::METHOD);
            }
        }
        MongoContext::FindCursor | MongoContext::AggregateCursor => {
            let methods: &[&str] = if *context == MongoContext::FindCursor {
                &mongo_shell::FIND_CURSOR_METHODS
            } else {
                &mongo_shell::AGGREGATE_CURSOR_METHODS
            };
            for method in methods {
                add(method, (*method).to_owned(), CompletionItemKind::METHOD);
            }
        }
        MongoContext::Stage => {
            let mut stages = words(safety::mongo_read_stages());
            if !scope.read_only {
                stages.extend(words(MONGO_WRITE_STAGES));
            }
            for stage in stages {
                add(&stage, stage.clone(), CompletionItemKind::KEYWORD);
            }
        }
        MongoContext::Operator => {
            let mut operators = words(MONGO_OPERATORS);
            if !scope.read_only {
                operators.extend(words(MONGO_UPDATE_OPERATORS));
            }
            for operator in operators {
                add(&operator, operator.clone(), CompletionItemKind::OPERATOR);
            }
        }
        MongoContext::Other => {
            for name in &names.columns {
                add(name, name.clone(), CompletionItemKind::FIELD);
            }
            for helper in MONGO_HELPERS {
                add(helper, (*helper).to_owned(), CompletionItemKind::FUNCTION);
            }
        }
        MongoContext::Nothing => {}
    }
    out.truncate(MAX_SUGGESTIONS);
    out
}

impl MongoCompletions {
    fn min_typed(
        &self,
        word_start: usize,
        context: &MongoContext,
        trigger: &CompletionContext,
    ) -> usize {
        if trigger.trigger_kind == CompletionTriggerKind::INVOKED {
            self.invoked_at.set(Some(word_start));
        } else if self.invoked_at.get() != Some(word_start) {
            self.invoked_at.set(None);
        }
        // After `db.` or `$` the menu opens at once: the choice is short and known.
        if self.invoked_at.get().is_some() || *context != MongoContext::Other {
            0
        } else {
            MIN_TYPED
        }
    }
}

impl CompletionProvider for MongoCompletions {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let empty = || Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        let all = text.to_string();
        let offset = offset.min(all.len());
        if !all.is_char_boundary(offset) {
            return empty();
        }
        // Only the statement being typed counts; one that doesn't split yet is read whole.
        let start = safety::split_statements(Adapter::Mongodb, &all[..offset])
            .last()
            .and_then(|last| {
                let base = all.as_ptr() as usize;
                let at = last.as_ptr() as usize - base;
                (at <= offset).then_some(at)
            })
            .unwrap_or(0);
        let before = &all[start..offset];
        let context = mongo_context(before);
        let prefix = ident_before(before);
        let word_start = offset - prefix.len();
        if prefix.chars().filter(|c| *c != '$').count()
            < self.min_typed(word_start, &context, &trigger)
        {
            return empty();
        }
        let range = lsp_types::Range {
            start: text.offset_to_position(word_start),
            end: text.offset_to_position(offset),
        };
        let items = mongo_suggestions(&context, prefix, &self.names.borrow(), &self.scope)
            .into_iter()
            .map(|(label, insert, kind)| CompletionItem {
                label,
                kind: Some(kind),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: insert,
                })),
                ..Default::default()
            })
            .collect();
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        new_text
            .chars()
            .next_back()
            .is_some_and(|c| is_word_char(c) || c == '.' || c == '$')
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CompletionContext, CompletionTriggerKind, MAX_SUGGESTIONS, MIN_TYPED, MongoContext,
        MongoScope, Names, SchemaNames, SqlCompletions, mongo_context, mongo_suggestions,
        suggestions, word_before,
    };

    fn labels(prefix: &str, names: &[&str]) -> Vec<String> {
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        suggestions(prefix, &names)
            .into_iter()
            .map(|(label, _)| label)
            .collect()
    }

    #[test]
    fn the_word_at_the_cursor_is_used() {
        assert_eq!(word_before("select na"), "na");
        assert_eq!(word_before("users.em"), "em");
        assert_eq!(word_before("select "), "");
    }

    #[test]
    fn database_names_come_before_keywords_ignoring_case() {
        assert_eq!(labels("SEL", &["selections"]), ["selections", "SELECT"]);
    }

    #[test]
    fn an_empty_word_gets_names_then_keywords() {
        let all = labels("", &["users"]);
        assert_eq!(all.len(), MAX_SUGGESTIONS);
        assert_eq!(all[..2], ["users", "ADD"]);
    }

    fn trigger(kind: CompletionTriggerKind) -> CompletionContext {
        CompletionContext {
            trigger_kind: kind,
            trigger_character: None,
        }
    }

    #[test]
    fn the_menu_waits_for_two_letters_unless_asked_for() {
        let completions = SqlCompletions::new(SchemaNames::default());
        let typed = trigger(CompletionTriggerKind::TRIGGER_CHARACTER);
        let invoked = trigger(CompletionTriggerKind::INVOKED);
        assert_eq!(completions.min_typed(7, &typed), MIN_TYPED);
        // Ctrl+Space, then typing the same word: the menu stays.
        assert_eq!(completions.min_typed(7, &invoked), 0);
        assert_eq!(completions.min_typed(7, &typed), 0);
        // A new word waits again, and so does the old one after it.
        assert_eq!(completions.min_typed(12, &typed), MIN_TYPED);
        assert_eq!(completions.min_typed(7, &typed), MIN_TYPED);
    }

    #[test]
    fn mongo_contexts_follow_the_command() {
        use MongoContext::*;
        let cases = [
            ("db.", Database(None)),
            ("db.us", Database(None)),
            ("  db . ", Database(None)),
            ("db.getSiblingDB('logs').", Database(Some("logs".into()))),
            ("db.users.", Collection),
            ("db.users.fi", Collection),
            ("db.system.views.", Collection),
            ("db.getCollection(\"my-coll\").", Collection),
            ("db.getSiblingDB(\"logs\").events.", Collection),
            ("db.users.find({a: 1}).", FindCursor),
            ("db.users.find().sort({a: -1}).li", FindCursor),
            ("db.users.find()\n  .limit(5).", FindCursor),
            ("db.users.aggregate([]).", AggregateCursor),
            ("db.users.countDocuments().", Nothing),
            ("db.users.find().explain().", Nothing),
            ("other.", Nothing),
            ("db.users.find({age: {$", Operator),
            ("db.users.find({$or: [{a: 1}, {$", Operator),
            ("db.users.aggregate([{$", Stage),
            ("db.users.aggregate([{$match: {a: 1}}, {$gr", Stage),
            ("db.users.aggregate([{$match: {a: {$g", Operator),
            ("db.users.aggregate([{$group: {_id: '$a', n: {$s", Operator),
            (
                "db.users.aggregate([{$lookup: {from: 'o', pipeline: [{$",
                Stage,
            ),
            ("db.users.aggregate([{$facet: {a: [{$", Stage),
            ("db.users.find({na", Other),
            ("db.users.find({name: 'db.", Nothing),
            ("db.users.find() // db.", Nothing),
            ("/* db. */ db.", Database(None)),
        ];
        for (text, context) in cases {
            assert_eq!(mongo_context(text), context, "{text:?}");
        }
    }

    fn mongo_labels(context: MongoContext, prefix: &str, read_only: bool) -> Vec<String> {
        let mut names = Names::default();
        names.set(
            vec![
                ("app".into(), "users".into()),
                ("app".into(), "order-items".into()),
                ("logs".into(), "events".into()),
            ],
            vec!["email".into(), "name".into()],
        );
        let scope = MongoScope {
            database: "app".into(),
            read_only,
        };
        mongo_suggestions(&context, prefix, &names, &scope)
            .into_iter()
            .map(|(label, insert, _)| {
                if label == insert {
                    label
                } else {
                    format!("{label} -> {insert}")
                }
            })
            .collect()
    }

    #[test]
    fn mongo_offers_the_collections_of_the_database_meant() {
        let labels = mongo_labels(MongoContext::Database(None), "", false);
        assert_eq!(
            labels[..3],
            [
                "users",
                "order-items -> getCollection(\"order-items\")",
                "getCollection"
            ]
        );
        assert!(labels.contains(&"runCommand".to_owned()));
        assert_eq!(
            mongo_labels(MongoContext::Database(Some("logs".into())), "e", false),
            ["events"]
        );
        // Read-only connections are refused runCommand, so it isn't offered.
        let read_only = mongo_labels(MongoContext::Database(None), "", true);
        assert!(
            !read_only.iter().any(|l| l.ends_with("Command")),
            "{read_only:?}"
        );
    }

    #[test]
    fn mongo_methods_and_stages_narrow_on_read_only_connections() {
        let writable = mongo_labels(MongoContext::Collection, "", false);
        assert_eq!(writable[..3], ["find", "findOne", "aggregate"]);
        assert!(writable.contains(&"insertOne".to_owned()));
        let read_only = mongo_labels(MongoContext::Collection, "", true);
        assert_eq!(
            read_only.len(),
            tuplebase_core::mongo_shell::READ_METHODS.len()
        );
        assert!(!read_only.contains(&"insertOne".to_owned()));
        assert_eq!(
            mongo_labels(MongoContext::Collection, "COUNT", true),
            ["countDocuments"]
        );

        let stages = mongo_labels(MongoContext::Stage, "$", true);
        assert_eq!(stages, tuplebase_core::safety::mongo_read_stages());
        assert!(mongo_labels(MongoContext::Stage, "$", false).contains(&"$merge".to_owned()));
        assert!(mongo_labels(MongoContext::Stage, "$o", true).is_empty());

        assert!(mongo_labels(MongoContext::Operator, "$in", false).contains(&"$inc".to_owned()));
        let mut operators = mongo_labels(MongoContext::Operator, "$", false);
        for letter in ["$w", "$f", "$a"] {
            operators.extend(mongo_labels(MongoContext::Operator, letter, false));
        }
        assert!(!mongo_labels(MongoContext::Operator, "$in", true).contains(&"$inc".to_owned()));
        for javascript in ["$where", "$function", "$accumulator"] {
            assert!(!operators.contains(&javascript.to_owned()));
        }
        assert_eq!(
            mongo_labels(MongoContext::FindCursor, "s", false),
            ["sort", "skip"]
        );
        assert_eq!(mongo_labels(MongoContext::Other, "na", false), ["name"]);
        assert_eq!(
            mongo_labels(MongoContext::Other, "Obj", false),
            ["ObjectId"]
        );
        assert!(mongo_labels(MongoContext::Nothing, "", false).is_empty());
    }
}
