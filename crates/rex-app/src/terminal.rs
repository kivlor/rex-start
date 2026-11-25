//! Paints a terminal frame as a grid of cells, sized to fill the bounds
//! the layout gave the block. The grid comes from the server; the client
//! only stretches it to its pane.

use std::sync::Arc;

use gpui::{
    fill, font, outline, point, px, rgb, size, App, Bounds, Element, ElementId, FontStyle,
    FontWeight, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, PaintQuad,
    Pixels, ShapedLine, SharedString, Style, TextRun, Window,
};
use rex_terminal::{Color, Frame};

/// The system's monospace font, resolved once and reused for every pane.
pub const MONO: &str = "DejaVu Sans Mono";

/// The design cell, in pixels: the size the non-stretch layout draws a
/// frame at (see `request_layout`). Grid negotiation asks for cols/rows
/// that fit a pane at this cell size, so the server's programs are
/// sized for how this client actually renders.
pub const CELL_W_PX: f32 = 9.0;
pub const CELL_H_PX: f32 = 18.0;

/// The grid that fits a pane of `pane_w` x `pane_h` pixels: floor
/// division by the cell size, never below 1x1 (the protocol's minimum)
/// and capped at the protocol's 65535.
pub fn grid_for(pane_w: f32, pane_h: f32) -> (u32, u32) {
    let cols = (pane_w.max(0.0) / CELL_W_PX).floor() as u32;
    let rows = (pane_h.max(0.0) / CELL_H_PX).floor() as u32;
    (cols.clamp(1, 65535), rows.clamp(1, 65535))
}

/// One block's terminal, ready to draw.
pub struct TerminalElement {
    frame: Arc<Frame>,
    /// Fill the pane instead of using the font's natural cell size.
    stretch: bool,
}

impl TerminalElement {
    pub fn new(frame: Arc<Frame>, stretch: bool) -> Self {
        Self { frame, stretch }
    }
}

pub struct PaintState {
    background: PaintQuad,
    cells: Vec<PaintQuad>,
    lines: Vec<ShapedLine>,
    cursor: Option<PaintQuad>,
    cell_h: Pixels,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = PaintState;

    fn id(&self) -> Option<ElementId> {
        // Elements with no id are fine; each pane is a fresh element.
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        core::panic::Location::caller().into()
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        if self.stretch {
            style.size.width = gpui::Length::Definite(gpui::DefiniteLength::Fraction(1.0));
            style.size.height = gpui::Length::Definite(gpui::DefiniteLength::Fraction(1.0));
        } else {
            style.size.width = px(f32::from(self.frame.cols) * CELL_W_PX).into();
            style.size.height = px(f32::from(self.frame.rows) * CELL_H_PX).into();
        }
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        let frame = &self.frame;
        let cell_w = bounds.size.width / f32::from(frame.cols.max(1));
        let cell_h = bounds.size.height / f32::from(frame.rows.max(1));
        // A font size that fits the cell height.
        let font_size = (f32::from(cell_h) * 0.8).clamp(4.0, 28.0);

        let cursor_at = |x: usize, y: usize| {
            frame
                .cursor
                .is_some_and(|c| c.visible && c.x as usize == x && c.y as usize == y)
        };

        let mut cells = Vec::new();
        let mut lines = Vec::with_capacity(frame.cells.len());
        for (y, row) in frame.cells.iter().enumerate() {
            // Runs of same-background cells become one quad.
            let mut start = 0;
            while start < row.len() {
                let background = row[start].background;
                let mut end = start + 1;
                while end < row.len() && row[end].background == background {
                    end += 1;
                }
                if background != frame.background {
                    cells.push(fill(
                        Bounds::new(
                            point(
                                bounds.left() + cell_w * start as f32,
                                bounds.top() + cell_h * y as f32,
                            ),
                            size(cell_w * (end - start) as f32, cell_h),
                        ),
                        color(background),
                    ));
                }
                start = end;
            }

            let mut text = String::with_capacity(row.len());
            let mut runs: Vec<TextRun> = Vec::with_capacity(row.len());
            for (x, cell) in row.iter().enumerate() {
                let display = if cell.text.is_empty() {
                    " "
                } else {
                    cell.text.as_str()
                };
                text.push_str(display);
                // The cursor cell: the character drawn in reverse.
                let foreground = if cursor_at(x, y) {
                    frame.background
                } else {
                    cell.foreground
                };
                let background = if cursor_at(x, y) {
                    frame.foreground
                } else {
                    cell.background
                };
                if background != frame.background {
                    cells.push(fill(
                        Bounds::new(
                            point(
                                bounds.left() + cell_w * x as f32,
                                bounds.top() + cell_h * y as f32,
                            ),
                            size(cell_w, cell_h),
                        ),
                        color(background),
                    ));
                }
                runs.push(text_run(foreground, display.len()));
            }
            let shaped = window.text_system().shape_line(
                SharedString::from(text),
                px(font_size),
                &runs,
                Some(cell_w),
            );
            lines.push(shaped);
        }

        // The cursor keeps an outline on top of everything, so the
        // pane's position in the grid stays visible.
        let cursor = frame.cursor.filter(|c| c.visible).map(|c| {
            outline(
                Bounds::new(
                    point(
                        bounds.left() + cell_w * f32::from(c.x),
                        bounds.top() + cell_h * f32::from(c.y),
                    ),
                    size(cell_w, cell_h),
                ),
                color(frame.foreground),
                gpui::BorderStyle::default(),
            )
        });

        PaintState {
            background: fill(bounds, color(frame.background)),
            cells,
            lines,
            cursor,
            cell_h,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_quad(state.background.clone());
        for cell in state.cells.drain(..) {
            window.paint_quad(cell);
        }
        for (y, line) in state.lines.iter().enumerate() {
            let origin = point(bounds.left(), bounds.top() + state.cell_h * y as f32);
            let _ = line.paint(origin, state.cell_h, window, cx);
        }
        if let Some(cursor) = state.cursor.take() {
            window.paint_quad(cursor);
        }
    }
}

fn text_run(foreground: Color, len: usize) -> TextRun {
    let mut font = font(MONO);
    font.weight = FontWeight::NORMAL;
    font.style = FontStyle::Normal;
    TextRun {
        len,
        font,
        color: Hsla::from(rgb((u32::from(foreground.r) << 16)
            | (u32::from(foreground.g) << 8)
            | u32::from(foreground.b))),
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

fn color(value: Color) -> gpui::Rgba {
    rgb((u32::from(value.r) << 16) | (u32::from(value.g) << 8) | u32::from(value.b))
}

#[cfg(test)]
mod tests {
    use super::{grid_for, CELL_H_PX, CELL_W_PX};

    #[test]
    fn exact_division_gives_the_exact_grid() {
        assert_eq!(grid_for(9.0 * 80.0, 18.0 * 24.0), (80, 24));
        assert_eq!(grid_for(CELL_W_PX, CELL_H_PX), (1, 1));
    }

    #[test]
    fn remainders_floor_never_round_up() {
        // 40 cols + a bit more than half a cell, 12 rows + most of a row.
        assert_eq!(grid_for(9.0 * 40.0 + 5.0, 18.0 * 12.0 + 17.0), (40, 12));
        // A hair under one cell is still nothing: clamped to the 1x1
        // minimum the protocol requires.
        assert_eq!(grid_for(8.9, 17.9), (1, 1));
    }

    #[test]
    fn tiny_and_degenerate_panes_clamp_to_one_cell() {
        // Smaller than a cell, exactly zero, or negative (a pane the
        // layout collapsed): always at least 1x1.
        assert_eq!(grid_for(0.0, 0.0), (1, 1));
        assert_eq!(grid_for(-100.0, -100.0), (1, 1));
        assert_eq!(grid_for(0.5, 0.5), (1, 1));
    }

    #[test]
    fn huge_panes_cap_at_the_protocol_maximum() {
        assert_eq!(grid_for(f32::MAX, f32::MAX), (65535, 65535));
    }

    #[test]
    fn a_split_pane_floor_matches_its_pixel_share() {
        // A 1100x694 window below a 26px header with a 3px gutter, split
        // 50/50 horizontally: the actor's math, end to end.
        let gutter = 3.0;
        let w = 1100.0f32 / 2.0 - gutter;
        let h = 694.0 - 26.0 - gutter;
        assert_eq!(grid_for(w, h), (60, 36)); // 547/9 and 665/18, floored
                                              // One full pane of the same window: 1097/9 and the same height.
        assert_eq!(grid_for(1100.0 - gutter, h), (121, 36));
    }
}
