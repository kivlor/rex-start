//! The GPUI window: one session's active window, its panes laid out the
//! way the server says, and a session picker. The client never invents
//! split geometry — every rectangle comes from the server's layout tree.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, rgb, Context, FocusHandle, KeyDownEvent, MouseButton, Pixels, Render,
    Window,
};
use rex_terminal::{row_text, Frame};

use crate::{
    client::{Client, ClientCmd, ClientEvent, Direction, KeyEvent},
    terminal::TerminalElement,
    view::{SessionBrief, SessionView},
};

/// The header bar's height and the gap drawn between panes.
const HEADER: f32 = 26.0;
const GUTTER: f32 = 3.0;

const WINDOW_BG: u32 = 0x141414;
const HEADER_BG: u32 = 0x1d1d1d;
const HEADER_FG: u32 = 0xbfbfbf;
const BORDER: u32 = 0x3a3a3a;
const FOCUS: u32 = 0x6f9cf5;
const PICKER_BG: u32 = 0x222222;
const PICKER_SEL: u32 = 0x3d4a78;

/// One step down from the picker list, for the hint line.
const KEYS: &str = "ctrl-alt-right split h \u{b7} ctrl-alt-down split v \u{b7} ctrl-alt-z zoom \u{b7} ctrl-alt-p sessions";

pub struct RexApp {
    focus_handle: FocusHandle,
    client: Option<Client>,
    sessions: Vec<SessionBrief>,
    session_id: Option<String>,
    session_label: Option<String>,
    view: Option<SessionView>,
    frames: HashMap<String, Arc<Frame>>,
    /// Blocks whose first frame was already dumped under REX_APP_DEBUG.
    debug_frames: Vec<String>,
    picker: Option<usize>,
    status: String,
}

impl RexApp {
    pub fn new(client: Client, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);
        let events = client.events.clone();
        let app = Self {
            focus_handle,
            client: Some(client),
            sessions: Vec::new(),
            session_id: None,
            session_label: None,
            view: None,
            frames: HashMap::new(),
            debug_frames: Vec::new(),
            picker: None,
            status: "connecting".into(),
        };
        // Pump client events into entity updates until the client ends.
        cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv().await {
                let alive = this.update(cx, |app, cx| app.on_client_event(event, cx));
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        app
    }

    fn on_client_event(&mut self, event: ClientEvent, cx: &mut Context<Self>) {
        match event {
            ClientEvent::Connected { sessions, current } => {
                self.sessions = sessions;
                if let Some((brief, view)) = current {
                    self.session_id = Some(brief.session_id.clone());
                    self.session_label = Some(brief.label.clone());
                    self.view = Some(view);
                }
                self.status = "connected".into();
            }
            ClientEvent::Sessions(sessions) => {
                self.sessions = sessions;
            }
            ClientEvent::View(brief, view) => {
                // A different session than the one we are showing: forget
                // the old session's frames before drawing the new view.
                if self.session_id.as_deref() != Some(brief.session_id.as_str()) {
                    self.frames.clear();
                    self.debug_frames.clear();
                    self.session_id = Some(brief.session_id.clone());
                    self.session_label = Some(brief.label.clone());
                }
                self.view = Some(view);
            }
            ClientEvent::CurrentGone => {
                self.view = None;
                self.session_id = None;
                self.session_label = None;
                self.frames.clear();
            }
            ClientEvent::Frame(block_id, frame) => {
                // Stale frames from a just-aborted stream of a previous
                // session or view: keep only blocks the view still has.
                if !self.panes().iter().any(|(id, _)| id == &block_id) {
                    return;
                }
                if std::env::var_os("REX_APP_DEBUG").is_some()
                    && !self.debug_frames.contains(&block_id)
                {
                    self.debug_frames.push(block_id.clone());
                    let rows: Vec<String> = frame
                        .cells
                        .iter()
                        .map(|row| row_text(row))
                        .filter(|row| !row.is_empty())
                        .take(4)
                        .collect();
                    eprintln!("[rex-app] {block_id} renders: {}", rows.join(" / "));
                }
                self.frames.insert(block_id, frame);
            }
            ClientEvent::Notice(text) => {
                self.status = text;
            }
            ClientEvent::Closed(reason) => {
                self.status = format!("disconnected: {reason}");
                self.client = None;
            }
        }
        cx.notify();
    }

    fn send(&self, cmd: ClientCmd) {
        if let Some(client) = &self.client {
            let _ = client.cmd.try_send(cmd);
        }
    }

    /// The active window's panes: block IDs with server-given rect
    /// fractions, mapped into the area below the header.
    fn panes(&self) -> Vec<(String, [f64; 4])> {
        self.view
            .as_ref()
            .and_then(SessionView::active_window)
            .map(|window| window.placements())
            .unwrap_or_default()
    }

    fn focused_block(&self) -> Option<String> {
        self.view
            .as_ref()
            .and_then(SessionView::active_window)
            .and_then(|w| w.focused_block_id.clone())
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let handled = if self.picker.is_some() {
            self.picker_key(keystroke)
        } else {
            self.main_key(keystroke, cx)
        };
        if handled {
            window.prevent_default();
            cx.stop_propagation();
            cx.notify();
        }
    }

    /// Keys for the session picker.
    fn picker_key(&mut self, keystroke: &gpui::Keystroke) -> bool {
        let count = self.sessions.len();
        let selected = self.picker.unwrap_or(0);
        match keystroke.key.as_str() {
            "escape" => {
                self.picker = None;
                true
            }
            "up" => {
                if count > 0 {
                    self.picker = Some(selected.min(count - 1).saturating_sub(1));
                }
                true
            }
            "down" => {
                if count > 0 {
                    self.picker = Some((selected + 1).min(count - 1));
                }
                true
            }
            "enter" => {
                self.picker = None;
                if let Some(brief) = self.sessions.get(selected) {
                    let id = brief.session_id.clone();
                    self.send(ClientCmd::SwitchSession(id));
                }
                true
            }
            "n" => {
                self.picker = None;
                self.send(ClientCmd::NewSession("rex-app".into()));
                true
            }
            _ => false,
        }
    }

    /// Keys for the session view: the app's own ctrl-alt-* bindings
    /// take precedence locally, ctrl+v pastes the clipboard, and every
    /// other typed key goes to the server through `client.key`, so the
    /// server's Lua bindings (init.lua) get first chance at it.
    fn main_key(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) -> bool {
        if keystroke.modifiers.control && keystroke.modifiers.alt {
            match keystroke.key.as_str() {
                "right" => {
                    self.send(ClientCmd::NewSplit {
                        direction: Direction::Horizontal,
                    });
                    return true;
                }
                "down" => {
                    self.send(ClientCmd::NewSplit {
                        direction: Direction::Vertical,
                    });
                    return true;
                }
                "z" => {
                    self.send(ClientCmd::ToggleZoom);
                    return true;
                }
                "p" => {
                    if self.picker.is_none() {
                        let selected = self
                            .session_id
                            .as_ref()
                            .and_then(|id| {
                                self.sessions.iter().position(|s| &s.session_id == id)
                            })
                            .unwrap_or(0);
                        self.picker = Some(selected);
                    } else {
                        self.picker = None;
                    }
                    return true;
                }
                "n" => {
                    self.send(ClientCmd::NewSession("rex-app".into()));
                    return true;
                }
                _ => {}
            }
        }
        // Paste: gpui 0.2 exposes no paste event, so ctrl+v reads the
        // clipboard and sends it as chunked client.key text.
        if keystroke.modifiers.control
            && !keystroke.modifiers.alt
            && keystroke.key == "v"
        {
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default();
            if !text.is_empty() {
                self.send(ClientCmd::Paste(text));
            }
            return true;
        }
        // Everything else the keyboard can say goes to the server's
        // focused block via client.key.
        match keystroke_to_key_event(keystroke) {
            Some(key) => {
                self.send(ClientCmd::SendKey { key, prefix: None });
                true
            }
            None => false,
        }
    }

    fn pane_at(&self, viewport: (f32, f32), x: f32, y: f32) -> gpui::Bounds<Pixels> {
        gpui::Bounds::new(
            gpui::point(px(x), px(y + HEADER)),
            gpui::size(px(viewport.0), px(viewport.1 - HEADER)),
        )
    }
}

impl Render for RexApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let (width, height) = (f32::from(viewport.width), f32::from(viewport.height));
        let area = self.pane_at((width, height), 0.0, 0.0);
        let focused = self.focused_block();

        let label = self
            .session_label
            .clone()
            .or_else(|| {
                self.session_id
                    .as_ref()
                    .and_then(|id| self.sessions.iter().find(|s| &s.session_id == id))
                    .map(|s| s.label.clone())
            })
            .unwrap_or_else(|| "no session".into());

        let header = div()
            .absolute()
            .left(px(0.0))
            .top(px(0.0))
            .w(px(width))
            .h(px(HEADER))
            .flex()
            .items_center()
            .px(px(10.0))
            .gap(px(12.0))
            .bg(rgb(HEADER_BG))
            .text_size(px(12.0))
            .text_color(rgb(HEADER_FG))
            .child(div().font_weight(gpui::FontWeight::BOLD).child(label))
            .child(div().child(self.status.clone()))
            .child(div().ml_auto().child(KEYS));

        let panes: Vec<gpui::AnyElement> = self
            .panes()
            .into_iter()
            .map(|(block_id, rect)| {
                let is_focused = focused.as_deref() == Some(block_id.as_str());
                let left = area.origin.x
                    + px(rect[0] as f32 * f32::from(area.size.width))
                    + px(GUTTER / 2.0);
                let top = area.origin.y
                    + px(rect[1] as f32 * f32::from(area.size.height))
                    + px(GUTTER / 2.0);
                let pane_w = (rect[2] as f32 * f32::from(area.size.width) - GUTTER).max(1.0);
                let pane_h = (rect[3] as f32 * f32::from(area.size.height) - GUTTER).max(1.0);
                let card = div()
                    .absolute()
                    .left(left)
                    .top(top)
                    .w(px(pane_w))
                    .h(px(pane_h))
                    .overflow_hidden()
                    .border_1()
                    .border_color(rgb(if is_focused { FOCUS } else { BORDER }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener({
                            let block_id = block_id.clone();
                            move |app, _: &gpui::MouseDownEvent, _, cx| {
                                app.send(ClientCmd::FocusBlock(block_id.clone()));
                                cx.notify();
                            }
                        }),
                    );
                let card = match self.frames.get(&block_id) {
                    Some(frame) => card.child(TerminalElement::new(frame.clone(), true)),
                    // Snapshot not restored yet.
                    None => card.child(
                        div()
                            .size_full()
                            .bg(rgb(WINDOW_BG))
                            .text_size(px(12.0))
                            .text_color(rgb(HEADER_FG))
                            .child("streaming\u{2026}"),
                    ),
                };
                card.into_any_element()
            })
            .collect();

        let empty = if panes.is_empty() {
            Some(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(14.0))
                    .text_color(rgb(HEADER_FG))
                    .child(div().child(match self.status.as_str() {
                        status if status.starts_with("disconnected") => status.to_owned(),
                        _ => "ctrl-alt-p to pick or create a session".to_owned(),
                    })),
            )
        } else {
            None
        };

        let mut root = div()
            .id("rex")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::key_down))
            .size_full()
            .bg(rgb(WINDOW_BG))
            .children(panes)
            .children(empty);
        if let Some(selected) = self.picker {
            root = root.child(self.render_picker(selected, (width, height)));
        }
        root.child(header)
    }
}

impl RexApp {
    /// The session picker: every session, the selected one highlighted.
    fn render_picker(&self, selected: usize, (_width, height): (f32, f32)) -> gpui::AnyElement {
        let card_w = 380.0;
        let row_h = 26.0;
        let rows = self.sessions.len().max(1);
        let card_h = (rows as f32 * row_h + 58.0).min(height - 80.0);
        let list: Vec<gpui::AnyElement> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(index, session)| {
                let current = self.session_id.as_deref() == Some(session.session_id.as_str());
                let title = if session.label.is_empty() {
                    session.session_id.clone()
                } else {
                    session.label.clone()
                };
                let title = if current {
                    format!("\u{25cf} {title}")
                } else {
                    title
                };
                div()
                    .w(px(card_w - 24.0))
                    .h(px(row_h))
                    .flex()
                    .items_center()
                    .px(px(12.0))
                    .rounded_sm()
                    .bg(rgb(if index == selected {
                        PICKER_SEL
                    } else {
                        PICKER_BG
                    }))
                    .text_size(px(13.0))
                    .text_color(rgb(0xe8e8e8))
                    .child(title)
                    .into_any_element()
            })
            .collect();
        div()
            .absolute()
            .size_full()
            .top(px(HEADER))
            .flex()
            .items_start()
            .justify_center()
            .pt(px(40.0))
            .child(
                div()
                    .w(px(card_w))
                    .h(px(card_h))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .p(px(12.0))
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PICKER_BG))
                    .shadow_lg()
                    .text_size(px(12.0))
                    .text_color(rgb(HEADER_FG))
                    .child(div().child("Sessions \u{2014} up/down select \u{b7} enter switch \u{b7} n new \u{b7} esc close"))
                    .children(list)
                    .into_any_element(),
            )
            .into_any_element()
    }
}

/// Maps a GPUI keystroke to a `client.key` key event: physical key as the
/// W3C KeyboardEvent code in snake_case, mods joined with `+`, the text
/// the key typed, and the unshifted code point that layout-dependent
/// bindings (ctrl+n and friends) match on.
///
/// Returns None for keys with no useful terminal meaning: bare modifier
/// presses, and anything gpui names that the mapping below doesn't know.
fn keystroke_to_key_event(keystroke: &gpui::Keystroke) -> Option<KeyEvent> {
    use gpui::Modifiers;

    let Modifiers {
        control,
        alt,
        shift,
        platform: super_,
        ..
    } = keystroke.modifiers;

    let mut mods = Vec::new();
    if shift {
        mods.push("shift");
    }
    if control {
        mods.push("ctrl");
    }
    if alt {
        mods.push("alt");
    }
    if super_ {
        mods.push("super");
    }
    // A key held by a control-ish modifier types nothing: the terminal
    // gets the control code via the physical key and mods instead.
    let chorded = control || alt || super_;

    // gpui names special keys the way its keybindings spell them.
    let (key, codepoint) = match keystroke.key.as_str() {
        "enter" | "return" => ("enter".to_owned(), 13),
        "escape" => ("escape".to_owned(), 27),
        "backspace" => ("backspace".to_owned(), 8),
        "tab" => ("tab".to_owned(), 9),
        "space" => ("space".to_owned(), 32),
        "left" => ("arrow_left".to_owned(), 0),
        "right" => ("arrow_right".to_owned(), 0),
        "up" => ("arrow_up".to_owned(), 0),
        "down" => ("arrow_down".to_owned(), 0),
        "home" => ("home".to_owned(), 0),
        "end" => ("end".to_owned(), 0),
        "pageup" => ("page_up".to_owned(), 0),
        "pagedown" => ("page_down".to_owned(), 0),
        "delete" => ("delete".to_owned(), 0),
        "insert" => ("insert".to_owned(), 0),
        key if key.len() == 1 => {
            let c = key.chars().next().expect("one char");
            let code = match c {
                'a'..='z' => format!("key_{c}"),
                '0'..='9' => format!("digit_{c}"),
                '-' => "minus".to_owned(),
                '.' => "period".to_owned(),
                ',' => "comma".to_owned(),
                '/' => "slash".to_owned(),
                ';' => "semicolon".to_owned(),
                '\'' => "quote".to_owned(),
                '[' => "bracket_left".to_owned(),
                ']' => "bracket_right".to_owned(),
                '\\' => "backslash".to_owned(),
                '`' => "backquote".to_owned(),
                '=' => "equal".to_owned(),
                // Unknown to the US layout: still send the text.
                _ => "unidentified".to_owned(),
            };
            // The code point the key types without shift, so ctrl+n
            // matches on layout. Arrows and friends have none (0).
            let unshifted = c.to_ascii_lowercase() as i32;
            (code, unshifted)
        }
        // Function keys, keypad keys: worth sending later; not mapped yet.
        _ => return None,
    };

    let text = if chorded {
        // A control-ish chord types nothing: the terminal gets the
        // control code via the physical key and mods instead.
        String::new()
    } else if keystroke.key == "space" {
        " ".to_owned()
    } else if keystroke.key.len() == 1 {
        // key_char is what the layout produces for this press, e.g. "S"
        // for shift+s, "ß" for alt+s on some layouts.
        keystroke
            .key_char
            .clone()
            .unwrap_or_else(|| keystroke.key.clone())
    } else {
        // Named keys (arrows, enter, escape, ...) type nothing.
        String::new()
    };

    Some(KeyEvent {
        key,
        mods: mods.join("+"),
        text,
        unshifted_codepoint: codepoint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keystroke(key: &str, key_char: Option<&str>, modifiers: gpui::Modifiers) -> gpui::Keystroke {
        gpui::Keystroke {
            modifiers,
            key: key.to_owned(),
            key_char: key_char.map(str::to_owned),
        }
    }

    #[test]
    fn plain_letter_maps_to_key_code_and_text() {
        let event = keystroke_to_key_event(&keystroke("a", Some("a"), gpui::Modifiers::default()))
            .expect("mappable");
        assert_eq!(event.key, "key_a");
        assert_eq!(event.mods, "");
        assert_eq!(event.text, "a");
        assert_eq!(event.unshifted_codepoint, 97);
    }

    #[test]
    fn shift_maps_text_but_keeps_unshifted_codepoint() {
        let event = keystroke_to_key_event(&keystroke(
            "s",
            Some("S"),
            gpui::Modifiers {
                shift: true,
                ..Default::default()
            },
        ))
        .expect("mappable");
        assert_eq!(event.key, "key_s");
        assert_eq!(event.mods, "shift");
        assert_eq!(event.text, "S");
        assert_eq!(event.unshifted_codepoint, 115);
    }

    #[test]
    fn ctrl_combo_loses_text_keeps_codepoint() {
        let event = keystroke_to_key_event(&keystroke(
            "c",
            None,
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        ))
        .expect("mappable");
        assert_eq!(event.key, "key_c");
        assert_eq!(event.mods, "ctrl");
        assert_eq!(event.text, "");
        assert_eq!(event.unshifted_codepoint, 99);
    }

    #[test]
    fn arrows_and_enter_map_to_protocol_names() {
        for (gpui_name, protocol_name) in [
            ("left", "arrow_left"),
            ("right", "arrow_right"),
            ("up", "arrow_up"),
            ("down", "arrow_down"),
        ] {
            let event = keystroke_to_key_event(&keystroke(gpui_name, None, Default::default()))
                .expect("mappable");
            assert_eq!(event.key, protocol_name);
            assert_eq!(event.text, "");
        }
        let event =
            keystroke_to_key_event(&keystroke("enter", None, Default::default())).expect("mappable");
        assert_eq!(event.key, "enter");
        assert_eq!(event.unshifted_codepoint, 13);
    }

    #[test]
    fn digits_and_punctuation_map_to_w3c_codes() {
        let event =
            keystroke_to_key_event(&keystroke("7", Some("7"), Default::default())).expect("mappable");
        assert_eq!(event.key, "digit_7");
        let event =
            keystroke_to_key_event(&keystroke("-", Some("-"), Default::default())).expect("mappable");
        assert_eq!(event.key, "minus");
        assert_eq!(event.text, "-");
    }
}
