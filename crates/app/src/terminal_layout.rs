//! Pure split-tree operations. Leaves are unique terminal identities (or saved tab indices).

use std::collections::HashSet;
use std::hash::Hash;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis { Horizontal, Vertical }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge { Left, Right, Top, Bottom }

impl Edge {
    fn placement(self) -> (Axis, bool) {
        match self {
            Self::Left => (Axis::Horizontal, true),
            Self::Right => (Axis::Horizontal, false),
            Self::Top => (Axis::Vertical, true),
            Self::Bottom => (Axis::Vertical, false),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layout<T> {
    Leaf(T),
    Split { axis: Axis, ratio: f32, first: Box<Self>, second: Box<Self> },
}

/// The edge nearest the pointer, provided it lies in the outer quarter. Ties are
/// resolved horizontally so corners have a deterministic target. Centre is not a drop target.
pub fn drop_edge(x: f32, y: f32, width: f32, height: f32) -> Option<Edge> {
    if !x.is_finite() || !y.is_finite() || !width.is_finite() || !height.is_finite()
        || width <= 0. || height <= 0. || x < 0. || y < 0. || x > width || y > height {
        return None;
    }
    let x = x / width;
    let y = y / height;
    let (horizontal, he) = if x <= 0.5 { (x, Edge::Left) } else { (1. - x, Edge::Right) };
    let (vertical, ve) = if y <= 0.5 { (y, Edge::Top) } else { (1. - y, Edge::Bottom) };
    if horizontal <= vertical && horizontal <= 0.25 { Some(he) }
    else if vertical <= 0.25 { Some(ve) } else { None }
}

/// Rectangle in logical pixels, relative to its containing surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    /// Split a rectangle without allowing a negative child size, even in a tiny window.
    pub fn split(self, axis: Axis, ratio: f32) -> (Self, Self) {
        let ratio = clamp_ratio(ratio);
        match axis {
            Axis::Horizontal => {
                let width = self.width.max(0.);
                let first = width * ratio;
                (
                    Self { width: first, ..self },
                    Self { x: self.x + first, width: width - first, ..self },
                )
            }
            Axis::Vertical => {
                let height = self.height.max(0.);
                let first = height * ratio;
                (
                    Self { height: first, ..self },
                    Self { y: self.y + first, height: height - first, ..self },
                )
            }
        }
    }

    pub fn edge_at(self, x: f32, y: f32) -> Option<Edge> {
        drop_edge(x - self.x, y - self.y, self.width, self.height)
    }

    pub fn half(self, edge: Edge) -> Self {
        let (axis, before) = edge.placement();
        let (first, second) = self.split(axis, 0.5);
        if before { first } else { second }
    }

    /// Convert a window-relative pointer position to a separator proportion.
    pub fn ratio_at(self, axis: Axis, x: f32, y: f32, divider: f32) -> Option<f32> {
        let (position, origin, size) = match axis {
            Axis::Horizontal => (x, self.x, self.width),
            Axis::Vertical => (y, self.y, self.height),
        };
        let available = size - divider;
        if !position.is_finite() || !origin.is_finite() || !available.is_finite() || available <= 0. {
            return None;
        }
        Some(clamp_ratio((position - origin - divider / 2.) / available))
    }
}

/// Clamp a separator to usable proportions, including corrupt persisted values.
pub fn clamp_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() { ratio.clamp(0.1, 0.9) } else { 0.5 }
}

impl<T: Copy + Eq + Hash> Layout<T> {
    pub fn contains(&self, id: T) -> bool {
        match self { Self::Leaf(value) => *value == id, Self::Split { first, second, .. } => first.contains(id) || second.contains(id) }
    }

    #[cfg(test)]
    pub fn leaves(&self, out: &mut Vec<T>) {
        match self { Self::Leaf(id) => out.push(*id), Self::Split { first, second, .. } => { first.leaves(out); second.leaves(out); } }
    }

    pub fn first_leaf(&self) -> T {
        match self {
            Self::Leaf(id) => *id,
            Self::Split { first, .. } => first.first_leaf(),
        }
    }

    /// Keep live leaves and collapse splits that have lost one side.
    pub fn retain_where(&mut self, mut valid: impl FnMut(T) -> bool) -> bool {
        self.retain(&mut valid).is_some()
    }

    pub fn remove(mut self, id: T) -> Option<Self> {
        self.retain(&mut |leaf| leaf != id).map(|_| self)
    }

    // Preserve the boxes of branches that survive. Return a surviving leaf to use
    // as a temporary value when moving the only surviving branch out of its box.
    fn retain(&mut self, valid: &mut impl FnMut(T) -> bool) -> Option<T> {
        let (keep_first, leaf) = match self {
            Self::Leaf(id) => return valid(*id).then_some(*id),
            Self::Split { ratio, first, second, .. } => match (first.retain(valid), second.retain(valid)) {
                (Some(id), Some(_)) => {
                    *ratio = clamp_ratio(*ratio);
                    return Some(id);
                }
                (Some(id), None) => (true, id),
                (None, Some(id)) => (false, id),
                (None, None) => return None,
            },
        };
        let Self::Split { first, second, .. } = std::mem::replace(self, Self::Leaf(leaf)) else {
            unreachable!("only splits can collapse")
        };
        *self = *if keep_first { first } else { second };
        Some(leaf)
    }

    /// Split a target with a terminal from this tree or a hidden tab. Invalid and
    /// self drops leave the tree unchanged.
    pub fn split(self, dragged: T, target: T, edge: Edge) -> Self {
        if dragged == target || !self.contains(target) { return self; }
        let mut root = self.remove(dragged).expect("target remains after removing another leaf");
        fn insert<T: Copy + Eq + Hash>(node: &mut Layout<T>, dragged: T, target: T, edge: Edge) -> bool {
            match node {
                Layout::Leaf(id) if *id == target => {
                    let (axis, before) = edge.placement();
                    let (first, second) = if before { (dragged, target) } else { (target, dragged) };
                    *node = Layout::Split { axis, ratio: 0.5, first: Box::new(Layout::Leaf(first)), second: Box::new(Layout::Leaf(second)) };
                    true
                }
                Layout::Leaf(_) => false,
                Layout::Split { first, second, .. } => insert(first, dragged, target, edge) || insert(second, dragged, target, edge),
            }
        }
        insert(&mut root, dragged, target, edge);
        root
    }

    /// Replace the current pane with a previously hidden terminal.
    pub fn replace(&mut self, old: T, new: T) -> bool {
        if self.contains(new) { return false; }
        fn visit<T: Copy + Eq>(node: &mut Layout<T>, old: T, new: T) -> bool {
            match node {
                Layout::Leaf(id) if *id == old => { *id = new; true }
                Layout::Leaf(_) => false,
                Layout::Split { first, second, .. } => visit(first, old, new) || visit(second, old, new),
            }
        }
        visit(self, old, new)
    }

    /// Translate identities while pruning absent and duplicate mappings.
    pub fn remap<U: Copy + Eq + Hash>(&self, mut map: impl FnMut(T) -> Option<U>) -> Option<Layout<U>> {
        fn visit<T: Copy, U: Copy + Eq + Hash>(node: &Layout<T>, map: &mut impl FnMut(T) -> Option<U>, seen: &mut HashSet<U>) -> Option<Layout<U>> {
            match node {
                Layout::Leaf(id) => map(*id).filter(|value| seen.insert(*value)).map(Layout::Leaf),
                Layout::Split { axis, ratio, first, second } => match (visit(first, map, seen), visit(second, map, seen)) {
                    (Some(first), Some(second)) => Some(Layout::Split { axis: *axis, ratio: clamp_ratio(*ratio), first: Box::new(first), second: Box::new(second) }),
                    (Some(node), None) | (None, Some(node)) => Some(node),
                    (None, None) => None,
                },
            }
        }
        visit(self, &mut map, &mut HashSet::new())
    }

    /// Address a branch by first/second child indices; leaves and invalid paths are ignored.
    pub fn resize(&mut self, path: &[bool], ratio: f32) -> bool {
        match (self, path.split_first()) {
            (Self::Split { ratio: current, .. }, None) => {
                let next = clamp_ratio(ratio);
                if *current == next { return false; }
                *current = next;
                true
            }
            (Self::Split { first, second, .. }, Some((right, rest))) => if *right { second.resize(rest, ratio) } else { first.resize(rest, ratio) },
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_geometry_and_invalid_coordinates() {
        assert_eq!(drop_edge(0., 0., 100., 100.), Some(Edge::Left));
        assert_eq!(drop_edge(50., 5., 100., 100.), Some(Edge::Top));
        assert_eq!(drop_edge(99., 50., 100., 100.), Some(Edge::Right));
        assert_eq!(drop_edge(50., 99., 100., 100.), Some(Edge::Bottom));
        assert_eq!(drop_edge(50., 50., 100., 100.), None);
        assert_eq!(drop_edge(-1., 1., 100., 100.), None);
        assert_eq!(drop_edge(f32::NAN, 1., 100., 100.), None);
        assert_eq!(drop_edge(0., 0., 0., 100.), None);
    }

    #[test]
    fn separator_geometry_clamps_and_handles_small_surfaces() {
        let rect = Rect { x: 10., y: 20., width: 100., height: 40. };
        assert_eq!(rect.edge_at(95., 40.), Some(Edge::Right));
        assert_eq!(rect.half(Edge::Right), Rect { x: 60., width: 50., ..rect });
        let (first, second) = rect.split(Axis::Horizontal, 0.25);
        assert_eq!(first.width, 25.);
        assert_eq!(second.x, 35.);
        assert_eq!(second.width, 75.);
        let (first, second) = rect.split(Axis::Vertical, f32::NAN);
        assert_eq!(first.height, 20.);
        assert_eq!(second.y, 40.);
        assert_eq!(rect.ratio_at(Axis::Horizontal, 0., 0., 4.), Some(0.1));
        assert_eq!(rect.ratio_at(Axis::Vertical, 0., 100., 4.), Some(0.9));
        assert_eq!(Rect { width: 4., ..rect }.ratio_at(Axis::Horizontal, 1., 1., 4.), None);
        assert_eq!(Rect { width: 104., ..rect }.ratio_at(Axis::Horizontal, 37., 20., 4.), Some(0.25));
    }

    #[test]
    fn split_move_prune_and_resize() {
        let root = Layout::Leaf(1).split(2, 1, Edge::Right).split(3, 2, Edge::Top);
        let mut ids = Vec::new(); root.leaves(&mut ids); assert_eq!(ids, vec![1, 3, 2]);
        let root = root.split(1, 2, Edge::Bottom);
        let mut ids = Vec::new(); root.leaves(&mut ids); assert_eq!(ids, vec![3, 2, 1]);
        assert_eq!(root.clone().split(1, 1, Edge::Left), root);
        assert_eq!(root.clone().split(4, 99, Edge::Left), root);
        let mut root = root.remove(2).unwrap();
        assert!(root.resize(&[], 2.));
        assert!(!root.resize(&[false], 0.5));
        assert!(matches!(root, Layout::Split { ratio, .. } if ratio == 0.9));
    }

    #[test]
    fn remap_prunes_missing_and_duplicate_leaves() {
        let tree = Layout::Leaf(1).split(2, 1, Edge::Right).split(3, 2, Edge::Bottom);
        assert_eq!(tree.remap(|id| match id { 1 => Some(9), 2 => None, _ => Some(9) }), Some(Layout::Leaf(9)));
        assert_eq!(tree.remap(|_| None::<u64>), None);
        let mapped = tree.remap(|id| Some(id + 10)).unwrap();
        let mut leaves = Vec::new(); mapped.leaves(&mut leaves); assert_eq!(leaves, vec![11, 12, 13]);
    }
}
