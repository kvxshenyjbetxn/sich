//! Commit graph lanes (port of the VS Code / Orca swimlane algorithm). Pure, no Git access.

/// Number of rotating lane colors.
pub const LANE_COLORS: u8 = 5;

/// A line of the graph that waits for the commit `id` to appear further down the log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lane {
    pub id: String,
    pub color: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRow {
    /// Lanes entering the row from above.
    pub input: Vec<Lane>,
    /// Lanes leaving the row downwards.
    pub output: Vec<Lane>,
    /// Lane index of the commit node.
    pub node_index: usize,
    pub is_head: bool,
    pub merge: bool,
    /// Number of parents of the commit.
    pub parent_count: usize,
    /// Color of the node: the lane it sits on, or a fresh one for a new tip.
    pub node_color: u8,
}

/// Incremental lane builder: feed commits in log order, page after page.
#[derive(Clone, Debug, Default)]
pub struct GraphBuilder {
    frontier: Vec<Lane>,
    next_color: u8,
}

impl GraphBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lanes still open after the last commit (the state carried across pages).
    pub fn frontier(&self) -> &[Lane] {
        &self.frontier
    }

    fn fresh_color(&mut self) -> u8 {
        let color = self.next_color % LANE_COLORS;
        self.next_color = (self.next_color + 1) % LANE_COLORS;
        color
    }

    pub fn push(&mut self, oid: &str, parents: &[String], is_head: bool) -> GraphRow {
        let input = self.frontier.clone();
        let node_index = input
            .iter()
            .position(|lane| lane.id == oid)
            .unwrap_or(input.len());
        let mut output = Vec::with_capacity(input.len() + parents.len());
        let mut matched = false;
        let mut node_color = 0;
        for lane in &input {
            if lane.id != oid {
                output.push(lane.clone());
            } else if !matched {
                matched = true;
                node_color = lane.color;
                if let Some(first) = parents.first() {
                    output.push(Lane {
                        id: first.clone(),
                        color: lane.color,
                    });
                }
            }
            // Other lanes waiting for this commit end here (they merge into the node).
        }
        if !matched {
            node_color = self.fresh_color();
            if let Some(first) = parents.first() {
                output.push(Lane {
                    id: first.clone(),
                    color: node_color,
                });
            }
        }
        for parent in parents.iter().skip(1) {
            let color = self.fresh_color();
            output.push(Lane {
                id: parent.clone(),
                color,
            });
        }
        self.frontier = output.clone();
        GraphRow {
            input,
            output,
            node_index,
            is_head,
            merge: parents.len() > 1,
            parent_count: parents.len(),
            node_color,
        }
    }
}

/// Rows for a whole list of `(oid, parents, is_head)`.
pub fn build_rows<'a>(
    commits: impl IntoIterator<Item = (&'a str, &'a [String], bool)>,
) -> Vec<GraphRow> {
    let mut builder = GraphBuilder::new();
    commits
        .into_iter()
        .map(|(oid, parents, head)| builder.push(oid, parents, head))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn ids(lanes: &[Lane]) -> Vec<&str> {
        lanes.iter().map(|l| l.id.as_str()).collect()
    }

    #[test]
    fn linear_history_keeps_one_lane_and_color() {
        let mut b = GraphBuilder::new();
        let r1 = b.push("c", &p(&["b"]), true);
        assert!(r1.input.is_empty());
        assert_eq!(r1.node_index, 0);
        assert!(r1.is_head && !r1.merge);
        assert_eq!(ids(&r1.output), ["b"]);
        let r2 = b.push("b", &p(&["a"]), false);
        assert_eq!(ids(&r2.input), ["b"]);
        assert_eq!(ids(&r2.output), ["a"]);
        assert_eq!(r2.node_index, 0);
        assert_eq!(r2.output[0].color, r1.output[0].color);
        assert_eq!(r2.node_color, r1.output[0].color);
        let r3 = b.push("a", &[], false);
        assert!(r3.output.is_empty());
        assert!(b.frontier().is_empty());
    }

    #[test]
    fn branch_and_merge() {
        // m merges f (feature) into main; both descend from base.
        let (m, f, a, base) = (p(&["a", "f"]), p(&["base"]), p(&["base"]), p(&[]));
        let rows = build_rows([
            ("m", m.as_slice(), true),
            ("f", f.as_slice(), false),
            ("a", a.as_slice(), false),
            ("base", base.as_slice(), false),
        ]);
        assert!(rows[0].merge);
        assert_eq!(ids(&rows[0].output), ["a", "f"]);
        assert_ne!(rows[0].output[0].color, rows[0].output[1].color);
        assert_eq!(rows[1].node_index, 1);
        assert_eq!(ids(&rows[1].output), ["a", "base"]);
        assert_eq!(rows[2].node_index, 0);
        assert_eq!(ids(&rows[2].output), ["base", "base"]);
        assert_eq!(rows[3].node_index, 0);
        assert!(rows[3].output.is_empty());
        assert_eq!(ids(&rows[3].input), ["base", "base"]);
    }

    #[test]
    fn octopus_merge_opens_a_lane_per_extra_parent() {
        let mut b = GraphBuilder::new();
        let r = b.push("m", &p(&["a", "b", "c"]), false);
        assert_eq!(ids(&r.output), ["a", "b", "c"]);
        let colors: Vec<_> = r.output.iter().map(|l| l.color).collect();
        assert_eq!(colors, [0, 1, 2]);
        assert!(r.merge);
    }

    #[test]
    fn multiple_heads_get_separate_lanes() {
        let mut b = GraphBuilder::new();
        let r1 = b.push("h1", &p(&["x"]), true);
        let r2 = b.push("h2", &p(&["y"]), false);
        assert_eq!(r1.node_index, 0);
        assert_eq!(r2.node_index, 1);
        assert_eq!(ids(&r2.input), ["x"]);
        assert_eq!(ids(&r2.output), ["x", "y"]);
        assert_ne!(r2.output[0].color, r2.output[1].color);
        let r3 = b.push("x", &p(&["z"]), false);
        assert_eq!(r3.node_index, 0);
        assert_eq!(ids(&r3.output), ["z", "y"]);
    }

    #[test]
    fn root_commits_end_their_lane_only() {
        let mut b = GraphBuilder::new();
        b.push("t1", &p(&["r1"]), false);
        b.push("t2", &p(&["r2"]), false);
        let r = b.push("r1", &[], false);
        assert_eq!(r.node_index, 0);
        assert_eq!(ids(&r.output), ["r2"]);
        let r = b.push("r2", &[], false);
        assert_eq!(r.node_index, 0);
        assert!(r.output.is_empty());
    }

    #[test]
    fn lone_root_commit_has_a_node_and_no_lines_out() {
        let mut b = GraphBuilder::new();
        let r = b.push("only", &[], true);
        assert_eq!(r.node_index, 0);
        assert!(r.output.is_empty());
        assert!(r.input.is_empty());
    }

    #[test]
    fn duplicate_waiting_lanes_collapse_into_the_first() {
        let mut b = GraphBuilder::new();
        b.push("m", &p(&["a", "b"]), false);
        b.push("a", &p(&["base"]), false);
        b.push("b", &p(&["base"]), false);
        let r = b.push("base", &p(&["older"]), false);
        assert_eq!(r.node_index, 0);
        assert_eq!(ids(&r.input), ["base", "base"]);
        assert_eq!(ids(&r.output), ["older"]);
        assert_eq!(r.output[0].color, r.input[0].color);
    }

    #[test]
    fn paging_continues_the_frontier() {
        let commits = [
            ("m", p(&["a", "f"]), false),
            ("f", p(&["base"]), false),
            ("a", p(&["base"]), false),
            ("base", p(&["root"]), false),
            ("root", p(&[]), false),
        ];
        let whole = build_rows(commits.iter().map(|(o, ps, h)| (*o, ps.as_slice(), *h)));
        let mut b = GraphBuilder::new();
        let mut paged = Vec::new();
        for (o, ps, h) in &commits[..2] {
            paged.push(b.push(o, ps, *h));
        }
        let mut resumed = b.clone();
        assert_eq!(ids(resumed.frontier()), ["a", "base"]);
        for (o, ps, h) in &commits[2..] {
            paged.push(resumed.push(o, ps, *h));
        }
        assert_eq!(paged, whole);
    }

    #[test]
    fn colors_rotate_over_five() {
        let mut b = GraphBuilder::new();
        let mut colors = Vec::new();
        for i in 0..7 {
            let id = format!("h{i}");
            let r = b.push(&id, &p(&[&format!("p{i}")]), false);
            colors.push(r.node_color);
        }
        assert_eq!(colors, [0, 1, 2, 3, 4, 0, 1]);
    }

    #[test]
    fn open_lanes_stay_in_the_frontier_at_page_end() {
        let mut b = GraphBuilder::new();
        b.push("c", &p(&["b"]), false);
        assert_eq!(ids(b.frontier()), ["b"]);
    }
}
