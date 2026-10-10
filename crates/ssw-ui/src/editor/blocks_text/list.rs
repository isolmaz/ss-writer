//! Bullet, numbered and task lists.
//!
//! The marker of an item is a filled dot at the top level, a ring one level deeper, a filled
//! square below that, then the cycle repeats; numbers are right-aligned in a column as wide as
//! the widest number; a task item shows a rounded checkbox instead (checked text is muted, never
//! struck through).

use ssw_core::markdown::ast::{Block, BlockKind, ListDelimiter};

use crate::canvas::Canvas;
use crate::editor::block::{BlockLayout, BlockPaintCx};
use crate::editor::config::EditorStrings;
use crate::text::TextStyle;

use super::item::{self, ItemSpec, Shape};
use super::stack::{
    self, Spec, Stack, forward_backdrop, forward_geometry, forward_links, forward_task_box,
};
use super::{Ctx, style};

/// A list: a stack of item layouts.
struct ListLayout {
    stack: Stack,
    label: String,
}

impl BlockLayout for ListLayout {
    fn height(&self) -> f32 {
        self.stack.bottom()
    }

    fn paint(&self, cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        self.stack.paint(cx, canvas);
    }

    forward_backdrop!(stack);
    forward_geometry!(stack);
    forward_task_box!(stack);
    forward_links!(stack);

    fn access_label(&self, _strings: &EditorStrings) -> String {
        self.label.clone()
    }
}

/// The layout of a list block (a stray list item is laid out as a one-item list).
pub(super) fn measure(ctx: &Ctx<'_>, block: &Block) -> Box<dyn BlockLayout> {
    let cx = ctx.cx;
    let tokens = &cx.theme.tokens;
    let (ordered, start, delimiter, items) = match &block.kind {
        BlockKind::List {
            ordered,
            start,
            delimiter,
            ..
        } => (
            *ordered,
            *start as usize,
            *delimiter,
            block.children.as_slice(),
        ),
        _ => (false, 0, ListDelimiter::Dash, std::slice::from_ref(block)),
    };
    let body = cx.config.body_size();
    let text_style = style(cx, &cx.config.text_font, body);
    let numbers: Vec<String> = if ordered {
        (0..items.len())
            .map(|index| format!("{}{}", start + index, delimiter.as_char()))
            .collect()
    } else {
        Vec::new()
    };
    let widest = numbers
        .iter()
        .map(|number| {
            cx.text
                .layout(number, &text_style, &[], &[], f32::INFINITY)
                .size()
                .width
        })
        .fold(0.0f32, f32::max);
    let col = (1.5 * body).max(widest + 0.9 * body);
    let tasks = items
        .iter()
        .any(|item| matches!(item.kind, BlockKind::ListItem { task: Some(_), .. }));
    let label = if tasks {
        ctx.strings.task_list.clone()
    } else if ordered {
        ctx.strings.numbered_list.clone()
    } else {
        ctx.strings.bullet_list.clone()
    };

    let list_ctx = Ctx {
        color: tokens.text,
        shown: ctx.shown || cx.reveal.touches(block.range),
        ..*ctx
    };
    let shared = Shared {
        numbers,
        text_style,
        col,
        label: &label,
    };
    let stack = stack::build(
        &list_ctx,
        &Spec {
            children: items,
            from: block.range.start,
            first_gap_line: cx.buffer.line_of(block.range.start),
            until: block.range.end,
            x: 0.0,
            y: 0.0,
            width: cx.width,
            show_prefix: list_ctx.shown,
        },
        &|item_ctx, index, item| measure_item(item_ctx, index, item, &shared),
    );
    Box::new(ListLayout { stack, label })
}

/// What every item of one list shares.
struct Shared<'a> {
    /// The numbers of an ordered list, by item index.
    numbers: Vec<String>,
    text_style: TextStyle,
    /// Width of the marker column.
    col: f32,
    label: &'a str,
}

/// Lays out item `index` of a list.
fn measure_item(
    ctx: &Ctx<'_>,
    index: usize,
    item: &Block,
    shared: &Shared<'_>,
) -> Box<dyn BlockLayout> {
    let task = match &item.kind {
        BlockKind::ListItem { task, .. } => *task,
        _ => None,
    };
    let shape = match (task, shared.numbers.get(index)) {
        (Some(task), _) => Shape::Check(task.checked),
        (None, Some(number)) => {
            Shape::Number(
                ctx.cx
                    .text
                    .layout(number, &shared.text_style, &[], &[], f32::INFINITY),
            )
        }
        (None, None) => match ctx.depth % 3 {
            0 => Shape::Dot,
            1 => Shape::Ring,
            _ => Shape::Square,
        },
    };
    Box::new(item::measure(
        ctx,
        ItemSpec {
            block: item,
            shape,
            col: shared.col,
            checkbox: task.map(|task| task.checkbox),
            checked: task.is_some_and(|task| task.checked),
            label: shared.label.to_owned(),
        },
    ))
}
