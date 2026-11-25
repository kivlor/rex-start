//! The session view: the server's own layout tree, parsed from
//! `session.view` / `session.bootstrap` results. The client never invents
//! geometry; everything here mirrors what the server reports.

use serde::Deserialize;

/// One session as `session.list` and `session.bootstrap` report it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SessionBrief {
    pub session_id: String,
    pub label: String,
}

/// A whole `session.view` result. Some fields are parsed for fidelity
/// with the protocol and aren't read yet.
#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
pub struct SessionView {
    pub session_id: Option<String>,
    pub label: Option<String>,
    pub revision: u64,
    pub active_window_id: Option<String>,
    #[serde(default)]
    pub windows: Vec<WindowView>,
}

/// One window (a tab) of a session.
#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
pub struct WindowView {
    pub window_id: Option<String>,
    pub label: Option<String>,
    pub focused_block_id: Option<String>,
    pub zoomed_block_id: Option<String>,
    #[serde(default)]
    pub layers: Vec<Layer>,
}

/// A tiled or floating layer of a window.
#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
pub struct Layer {
    pub layer_id: Option<String>,
    pub kind: String,
    pub bounds: Option<[f64; 4]>,
    pub layout: Option<LayoutNode>,
    #[serde(default)]
    pub blocks: Vec<BlockRef>,
    pub state: Option<String>,
}

/// A node of the layout tree: either one block or a split with two sides.
#[derive(Clone, Debug, Deserialize)]
pub struct LayoutNode {
    /// Where this node sits, as [x, y, w, h] fractions of the window.
    pub rect: [f64; 4],
    pub block_id: Option<String>,
    pub split: Option<Split>,
}

/// The two sides of a split.
#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
pub struct Split {
    pub direction: String,
    #[serde(default)]
    pub ratio: f64,
    pub split_id: Option<String>,
    pub before: Box<LayoutNode>,
    pub after: Box<LayoutNode>,
}

/// A block as the view lists it.
#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
pub struct BlockRef {
    pub block_id: Option<String>,
    pub flavor: Option<String>,
    pub creator_name: Option<String>,
    pub label: Option<String>,
}

impl LayoutNode {
    /// Walks the tree's leaves in layout order, reporting each block and
    /// the rect fractions the server gave it.
    pub fn leaves(&self, visit: &mut dyn FnMut(&str, [f64; 4])) {
        if let Some(split) = &self.split {
            split.before.leaves(visit);
            split.after.leaves(visit);
        } else if let Some(block_id) = &self.block_id {
            visit(block_id, self.rect);
        }
    }
}

impl Layer {
    /// True when the layer shouldn't be drawn.
    pub fn is_minimized(&self) -> bool {
        self.state.as_deref() == Some("minimized")
    }

    /// The terminal blocks of this layer, in layout order. Only terminals
    /// have a stream to draw; anything else the server adds later is
    /// skipped, not drawn wrong.
    pub fn terminal_block_ids(&self) -> Vec<&str> {
        self.blocks
            .iter()
            .filter(|block| {
                block
                    .creator_name
                    .as_deref()
                    .is_some_and(|name| name.starts_with("com.superlogical.terminal"))
            })
            .filter_map(|block| block.block_id.as_deref())
            .collect()
    }
}

impl WindowView {
    /// The terminal blocks to draw, in layout order, with the rect
    /// fractions the server assigned each one. A zoomed block covers the
    /// whole window; layouts keep their normal sizes underneath.
    pub fn placements(&self) -> Vec<(String, [f64; 4])> {
        if let Some(zoomed) = &self.zoomed_block_id {
            return vec![(zoomed.clone(), [0.0, 0.0, 1.0, 1.0])];
        }
        let mut placed = Vec::new();
        for layer in self.layers.iter().filter(|l| !l.is_minimized()) {
            let bounds = layer.bounds.unwrap_or([0.0, 0.0, 1.0, 1.0]);
            let scale = |rect: [f64; 4]| {
                [
                    bounds[0] + rect[0] * bounds[2],
                    bounds[1] + rect[1] * bounds[3],
                    rect[2] * bounds[2],
                    rect[3] * bounds[3],
                ]
            };
            if let Some(layout) = &layer.layout {
                let terminals: Vec<&str> = layer.terminal_block_ids();
                layout.leaves(&mut |block_id, rect| {
                    if terminals.contains(&block_id) {
                        placed.push((block_id.to_owned(), scale(rect)));
                    }
                });
            }
        }
        placed
    }
}

impl SessionView {
    /// The session's active window, the one it shows. A session with no
    /// active window named still shows its first window.
    pub fn active_window(&self) -> Option<&WindowView> {
        match self.active_window_id.as_deref() {
            Some(id) => self
                .windows
                .iter()
                .find(|w| w.window_id.as_deref() == Some(id)),
            None => self.windows.first(),
        }
    }
}

/// Parses `session.bootstrap`'s result enough to start the UI.
#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
pub struct Bootstrap {
    pub client_id: Option<String>,
    pub attach_all: AttachAll,
    #[serde(default)]
    pub attached_sessions: Vec<Attached>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AttachAll {
    #[serde(default)]
    pub sessions: Vec<SessionBrief>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Attached {
    pub session: SessionBrief,
    pub view: SessionView,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_two_pane_layout_in_order() {
        // From `session.view` against the local server: one window, one
        // tiled layer, split 0.5 horizontally.
        let json = serde_json::json!({
            "session_id": "session:1", "label": "demo", "revision": 4,
            "active_window_id": "window:1",
            "windows": [{
                "window_id": "window:1", "focused_block_id": "block:2",
                "layers": [{
                    "layer_id": "layer:1", "kind": "tiled",
                    "bounds": [0.0, 0.0, 1.0, 1.0],
                    "blocks": [
                        {"block_id": "block:1", "creator_name": "com.superlogical.terminal",
                         "flavor": "com.superlogical.terminal.shell"},
                        {"block_id": "block:2", "creator_name": "com.superlogical.terminal"}
                    ],
                    "layout": {
                        "rect": [0.0, 0.0, 1.0, 1.0],
                        "split": {
                            "direction": "horizontal", "ratio": 0.5, "split_id": "split:1",
                            "before": {"rect": [0.0, 0.0, 0.5, 1.0], "block_id": "block:1"},
                            "after":  {"rect": [0.5, 0.0, 0.5, 1.0], "block_id": "block:2"}
                        }
                    }
                }]
            }]
        });
        let view: SessionView = serde_json::from_value(json).unwrap();
        let window = view.active_window().unwrap();
        assert_eq!(window.focused_block_id.as_deref(), Some("block:2"));
        assert_eq!(
            window.placements(),
            vec![
                ("block:1".to_owned(), [0.0, 0.0, 0.5, 1.0]),
                ("block:2".to_owned(), [0.5, 0.0, 0.5, 1.0]),
            ]
        );
    }

    #[test]
    fn a_nested_split_reports_fraction_rects_from_the_server() {
        let json = serde_json::json!({
            "session_id": "session:1", "revision": 1,
            "windows": [{
                "window_id": "window:1",
                "layers": [{
                    "layer_id": "layer:1", "kind": "tiled",
                    "bounds": [0.0, 0.0, 1.0, 1.0],
                    "blocks": [
                        {"block_id": "a", "creator_name": "com.superlogical.terminal"},
                        {"block_id": "b", "creator_name": "com.superlogical.terminal"},
                        {"block_id": "c", "creator_name": "com.superlogical.terminal"}
                    ],
                    "layout": {
                        "rect": [0.0, 0.0, 1.0, 1.0],
                        "split": {
                            "direction": "vertical", "ratio": 0.5, "split_id": "s",
                            "before": {"rect": [0.0, 0.0, 1.0, 0.5],
                                       "block_id": "a"},
                            "after": {"rect": [0.0, 0.5, 1.0, 0.5],
                                      "split": {
                                "direction": "horizontal", "ratio": 0.5, "split_id": "s2",
                                "before": {"rect": [0.0, 0.5, 0.5, 0.5], "block_id": "b"},
                                "after":  {"rect": [0.5, 0.5, 0.5, 0.5], "block_id": "c"}
                            }}
                        }
                    }
                }]
            }]
        });
        let view: SessionView = serde_json::from_value(json).unwrap();
        let window = view.active_window().unwrap();
        let ids: Vec<String> = window.placements().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        let rects: Vec<[f64; 4]> = window
            .placements()
            .into_iter()
            .map(|(_, rect)| rect)
            .collect();
        // The server's leaf rects, untouched: b sits inside the after half.
        assert_eq!(rects[1], [0.0, 0.5, 0.5, 0.5]);
    }

    #[test]
    fn zoom_covers_the_whole_window() {
        let json = serde_json::json!({
            "session_id": "session:1", "revision": 2,
            "windows": [{
                "window_id": "window:1", "zoomed_block_id": "block:2",
                "layers": [{
                    "layer_id": "layer:1", "kind": "tiled",
                    "bounds": [0.0, 0.0, 1.0, 1.0],
                    "blocks": [{"block_id": "block:1"}, {"block_id": "block:2"}],
                    "layout": {
                        "rect": [0.0, 0.0, 1.0, 1.0],
                        "split": {
                            "direction": "horizontal", "ratio": 0.5, "split_id": "s",
                            "before": {"rect": [0.0, 0.0, 0.5, 1.0], "block_id": "block:1"},
                            "after":  {"rect": [0.5, 0.0, 0.5, 1.0], "block_id": "block:2"}
                        }
                    }
                }]
            }]
        });
        let view: SessionView = serde_json::from_value(json).unwrap();
        let window = view.active_window().unwrap();
        assert_eq!(
            window.placements(),
            vec![("block:2".to_owned(), [0.0, 0.0, 1.0, 1.0])]
        );
    }

    #[test]
    fn skips_non_terminal_blocks_and_minimized_layers() {
        let json = serde_json::json!({
            "session_id": "session:1", "revision": 3,
            "windows": [{
                "window_id": "window:1",
                "layers": [{
                    "layer_id": "layer:1", "kind": "tiled", "state": "minimized",
                    "bounds": [0.0, 0.0, 1.0, 1.0],
                    "blocks": [{"block_id": "hidden"}],
                    "layout": {"rect": [0.0, 0.0, 1.0, 1.0], "block_id": "hidden"}
                }, {
                    "layer_id": "layer:2", "kind": "tiled",
                    "bounds": [0.0, 0.0, 1.0, 1.0],
                    "blocks": [
                        {"block_id": "term", "creator_name": "com.superlogical.terminal"},
                        {"block_id": "other", "creator_name": "com.superlogical.onboarding"}
                    ],
                    "layout": {
                        "rect": [0.0, 0.0, 1.0, 1.0],
                        "split": {
                            "direction": "horizontal", "ratio": 0.5, "split_id": "s",
                            "before": {"rect": [0.0, 0.0, 0.5, 1.0], "block_id": "term"},
                            "after":  {"rect": [0.5, 0.0, 0.5, 1.0], "block_id": "other"}
                        }
                    }
                }]
            }]
        });
        let view: SessionView = serde_json::from_value(json).unwrap();
        let window = view.active_window().unwrap();
        assert_eq!(
            window.placements(),
            vec![("term".to_owned(), [0.0, 0.0, 0.5, 1.0])]
        );
    }

    #[test]
    fn parses_bootstrap_sessions_and_the_attached_view() {
        let json = serde_json::json!({
            "client_id": "client:1",
            "attach_all": {"sessions": [
                {"session_id": "session:1", "label": "work"},
                {"session_id": "session:2", "label": "scratch"}
            ]},
            "attached_sessions": [{
                "session": {"session_id": "session:1", "label": "work"},
                "view": {
                    "session_id": "session:1", "label": "work", "revision": 7,
                    "active_window_id": "window:1",
                    "windows": [{
                        "window_id": "window:1",
                        "layers": [{
                            "layer_id": "layer:1", "kind": "tiled",
                            "bounds": [0.0, 0.0, 1.0, 1.0],
                            "blocks": [{"block_id": "block:1",
                                        "creator_name": "com.superlogical.terminal"}],
                            "layout": {"rect": [0.0, 0.0, 1.0, 1.0],
                                       "block_id": "block:1"}
                        }]
                    }]
                }
            }]
        });
        let bootstrap: Bootstrap = serde_json::from_value(json).unwrap();
        assert_eq!(
            bootstrap.attach_all.sessions,
            vec![
                SessionBrief {
                    session_id: "session:1".into(),
                    label: "work".into()
                },
                SessionBrief {
                    session_id: "session:2".into(),
                    label: "scratch".into()
                },
            ]
        );
        let attached = &bootstrap.attached_sessions[0];
        assert_eq!(attached.session.session_id, "session:1");
        assert_eq!(attached.view.revision, 7);
        assert_eq!(
            attached.view.active_window().unwrap().placements(),
            vec![("block:1".to_owned(), [0.0, 0.0, 1.0, 1.0])]
        );
    }
}
