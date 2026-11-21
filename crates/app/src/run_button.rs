//! The Run button beside the statement under the cursor in the SQL editor, and the bar in
//! the gutter that marks that statement's lines.

use std::ops::Range;

use gpui_kit::component::button::*;
use gpui_kit::component::input::{EditorState, GutterDecoration, GutterDecorationCollection};
use gpui_kit::component::*;
use gpui_kit::*;
use tuplebase_core::config::Adapter;
use tuplebase_core::safety;

#[cfg(target_os = "macos")]
const HINT: &str = "Run statement (⌘↵)";
#[cfg(not(target_os = "macos"))]
const HINT: &str = "Run statement (Ctrl+Enter)";

/// Space between the end of the statement's first line and the button.
const GAP: Pixels = px(12.);
/// About as wide as the button, to tell whether it fits after the line.
const BUTTON_WIDTH: Pixels = px(64.);
/// Kept clear at the editor's right edge, where its scrollbar is.
const RIGHT_MARGIN: Pixels = px(16.);

/// Asks to run one statement; holds its text.
pub struct RunStatement(pub String);

/// The statement is found the way a script is split (`safety::statement_at`), so the
/// button runs exactly the piece a whole-script run would.
pub struct RunButton {
    editor: Entity<EditorState>,
    adapter: Adapter,
    statement: Option<Statement>,
    marks: GutterDecorationCollection,
    _editor_changes: Subscription,
}

#[derive(Clone, PartialEq)]
struct Statement {
    range: Range<usize>,
    /// The end of its first line, where the button goes.
    first_line_end: usize,
    /// That line's number, counted from 0.
    first_line: usize,
}

impl EventEmitter<RunStatement> for RunButton {}

impl RunButton {
    pub fn new(editor: Entity<EditorState>, cx: &mut Context<Self>) -> Self {
        let marks = editor.update(cx, |editor, cx| {
            editor.create_gutter_decorations_collection(Vec::new(), cx)
        });
        // The editor notifies when its text or cursor changes, and after a paint that moved
        // its layout, which is what the button's position comes from.
        let _editor_changes = cx.observe(&editor, |this, _, cx| this.refresh(cx));
        let mut this = Self {
            editor,
            adapter: Adapter::Postgres,
            statement: None,
            marks,
            _editor_changes,
        };
        this.refresh(cx);
        this
    }

    /// The engine whose rules split the editor's text.
    pub fn set_adapter(&mut self, adapter: Adapter, cx: &mut Context<Self>) {
        if self.adapter != adapter {
            self.adapter = adapter;
            self.refresh(cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let editor = self.editor.read(cx);
        let text = editor.value();
        let statement =
            safety::statement_at(self.adapter, &text, editor.cursor()).map(|range| Statement {
                first_line_end: text[range.clone()]
                    .find('\n')
                    .map_or(range.end, |i| range.start + i),
                first_line: text[..range.start].matches('\n').count(),
                range,
            });
        if statement != self.statement {
            let color = cx.theme().border;
            let marks = statement
                .iter()
                .map(|statement| GutterDecoration::new(statement.range.clone()).with_color(color))
                .collect();
            self.marks.set(marks, cx);
            self.statement = statement;
        }
        cx.notify();
    }

    fn run(&mut self, cx: &mut Context<Self>) {
        if let Some(statement) = &self.statement {
            let text = self.editor.read(cx).value();
            cx.emit(RunStatement(text[statement.range.clone()].to_owned()));
        }
    }

    /// Where the button goes, in window coordinates: after the statement's first line, or
    /// at the editor's right edge when the line is too long. `None` while that line is
    /// scrolled out of view or not laid out yet.
    fn position(&self, cx: &App) -> Option<(Point<Pixels>, Anchor, Pixels)> {
        let statement = self.statement.as_ref()?;
        let editor = self.editor.read(cx);
        // Bounds are only right for laid-out lines: an offset above them reads as the
        // first visible line's start.
        if !editor.visible_row_range()?.contains(&statement.first_line) {
            return None;
        }
        let end = statement.first_line_end;
        let line = editor.range_to_bounds(&(end..end))?;
        let line_height = editor.line_height()?;
        let view = editor.input_bounds();
        if line.top() < view.top() || line.top() + line_height > view.bottom() {
            return None;
        }
        let right = view.right() - RIGHT_MARGIN;
        let x = line.left() + GAP;
        Some(if x + BUTTON_WIDTH <= right {
            (point(x, line.top()), Anchor::TopLeft, line_height)
        } else {
            (point(right, line.top()), Anchor::TopRight, line_height)
        })
    }
}

impl Render for RunButton {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((position, anchor, line_height)) = self.position(cx) else {
            return Empty.into_any_element();
        };
        let muted = cx.theme().muted_foreground;
        anchored()
            .position(position)
            .anchor(anchor)
            .child(
                h_flex().h(line_height).items_center().child(
                    Button::new("run-statement")
                        .cursor_pointer()
                        .ghost()
                        .xsmall()
                        .icon(IconName::Play)
                        .label("Run")
                        .text_color(muted)
                        .tooltip(HINT)
                        .on_click(cx.listener(|this, _, _, cx| this.run(cx))),
                ),
            )
            .into_any_element()
    }
}
