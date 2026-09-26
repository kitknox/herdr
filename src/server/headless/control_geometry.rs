//! Protocol 1/2 viewers need a cell-space projection of independently sized
//! terminals. Protocol 3 keeps the original layout and a separate terminal grid.

use crate::api::schema::{
    PaneLayoutPane, PaneLayoutRect, PaneLayoutSnapshot, PaneLayoutSplit, SplitDirection,
};
use crate::layout::{Node, PaneId};
use ratatui::layout::Direction;
use std::collections::HashMap;

struct Projection {
    width: u16,
    height: u16,
    panes: Vec<PaneLayoutPane>,
    splits: Vec<PaneLayoutSplit>,
}

impl Projection {
    fn translate(&mut self, x: u16, y: u16) {
        for rect in self
            .panes
            .iter_mut()
            .map(|pane| &mut pane.rect)
            .chain(self.splits.iter_mut().map(|split| &mut split.rect))
        {
            rect.x = rect.x.saturating_add(x);
            rect.y = rect.y.saturating_add(y);
        }
    }
}

pub(super) fn legacy_layout(
    mut layout: PaneLayoutSnapshot,
    root: &Node,
    public_id: impl Fn(PaneId) -> Option<String>,
) -> PaneLayoutSnapshot {
    if layout.panes.iter().all(|pane| pane.terminal_size.is_none()) {
        return layout;
    }
    fn leaf(pane: &PaneLayoutPane) -> Projection {
        let mut pane = pane.clone();
        if let Some(size) = pane.terminal_size.take() {
            pane.rect.width = size.cols;
            pane.rect.height = size.rows;
        }
        pane.rect.x = 0;
        pane.rect.y = 0;
        Projection {
            width: pane.rect.width,
            height: pane.rect.height,
            panes: vec![pane],
            splits: vec![],
        }
    }
    fn pack(
        node: &Node,
        path: &str,
        panes: &HashMap<&str, &PaneLayoutPane>,
        public_id: &impl Fn(PaneId) -> Option<String>,
    ) -> Option<Projection> {
        match node {
            Node::Pane(id) => {
                let id = public_id(*id)?;
                panes.get(id.as_str()).map(|pane| leaf(pane))
            }
            Node::Split {
                direction,
                first,
                second,
                ..
            } => {
                let a = pack(first, &format!("{path}0"), panes, public_id);
                let b = pack(second, &format!("{path}1"), panes, public_id);
                let (mut a, mut b) = match (a, b) {
                    (Some(a), Some(b)) => (a, b),
                    (a, b) => return a.or(b),
                };
                let horizontal = *direction == Direction::Horizontal;
                let (width, height, ratio) = if horizontal {
                    let width = a.width.saturating_add(b.width);
                    b.translate(a.width, 0);
                    (
                        width,
                        a.height.max(b.height),
                        f32::from(a.width) / f32::from(width.max(1)),
                    )
                } else {
                    let height = a.height.saturating_add(b.height);
                    b.translate(0, a.height);
                    (
                        a.width.max(b.width),
                        height,
                        f32::from(a.height) / f32::from(height.max(1)),
                    )
                };
                a.panes.extend(b.panes);
                a.splits.extend(b.splits);
                a.splits.push(PaneLayoutSplit {
                    id: path.to_owned(),
                    direction: if horizontal {
                        SplitDirection::Right
                    } else {
                        SplitDirection::Down
                    },
                    ratio,
                    rect: PaneLayoutRect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    },
                });
                a.width = width;
                a.height = height;
                Some(a)
            }
        }
    }
    let projection = if layout.zoomed {
        layout.panes.first().map(leaf)
    } else {
        let panes = layout
            .panes
            .iter()
            .map(|pane| (pane.pane_id.as_str(), pane))
            .collect();
        pack(root, "", &panes, &public_id)
    };
    if let Some(projection) = projection {
        layout.area = PaneLayoutRect {
            x: 0,
            y: 0,
            width: projection.width,
            height: projection.height,
        };
        layout.panes = projection.panes;
        let original = &layout.splits;
        layout.splits = projection
            .splits
            .into_iter()
            .map(|mut split| {
                let path = if split.id.is_empty() {
                    "root"
                } else {
                    &split.id
                };
                if let Some(existing) = original
                    .iter()
                    .find(|existing| existing.id.rsplit('_').next() == Some(path))
                {
                    split.id = existing.id.clone();
                }
                split
            })
            .collect();
    }
    layout
}
