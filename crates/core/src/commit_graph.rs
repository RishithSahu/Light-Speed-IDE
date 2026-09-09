//! Lane assignment for the Source Control panel's commit graph.
//!
//! `git log` (parsed by [`crate::git::parse_log`]) hands back commits in one
//! flat, newest-first list -- exactly the order a straight line already
//! rendered them in. What it does not hand back on its own is *branch
//! topology*: which commits share a parent, where a branch forked off, where
//! a merge brought two lines back together. That is what `%P` (each commit's
//! parent hashes, now parsed onto [`crate::git::CommitLogEntry::parents`])
//! encodes, and what [`lanes`] turns into columns a renderer can draw --
//! the same shape `git log --graph`, `gitk` and every IDE's Source Control
//! graph view compute, by the same method: walk the list once, keep a
//! per-lane "which commit do I expect here next" table, and place each
//! commit in whichever lane (if any) was already expecting it.

use crate::git::CommitLogEntry;

/// One commit's place in the graph: which column it sits in, and which
/// column-to-column segments to draw from this row down to the next one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphRow {
    /// The lane (0-based column) this commit's own dot is drawn in.
    pub lane: usize,
    /// Segments to draw between this row and the next, `(from_lane,
    /// to_lane)`. `from == to` is a straight vertical continuation (an
    /// unrelated branch just passing through, or this commit's own line
    /// continuing to its first parent in the same lane); `from != to` is a
    /// diagonal -- a branch forking away from this commit, or a merge
    /// bringing two lines together. Order is not meaningful; a renderer
    /// draws every entry.
    pub edges: Vec<(usize, usize)>,
}

/// Assigns a lane and edge set to every commit in `commits` (newest first,
/// [`crate::git::parse_log`]'s own order). One [`GraphRow`] per commit, same
/// length and order as `commits`.
///
/// # The algorithm
///
/// `active[lane]` tracks which commit hash that lane currently expects to
/// see next (reading top to bottom, i.e. newest to oldest). For each commit
/// in turn:
/// 1. If some lane already expects this commit's hash, that is its lane --
///    a normal continuation, or the point two diverged lines re-converge.
///    Any *other* lane that also expected this hash (a fast-forward join
///    with no explicit merge commit) converges into the same lane too.
/// 2. Otherwise it is a fresh branch tip (`HEAD`, or a branch with no commit
///    yet linking it to what came before it in this bounded log) and takes
///    the first free lane, or a new one past the end.
/// 3. The commit's first parent continues in this commit's own lane; every
///    additional parent (a merge) either reconnects to a lane already
///    expecting it or claims a new one -- both produce a diagonal edge.
/// 4. Every other lane that was active and untouched by the above simply
///    continues straight through this row.
pub fn lanes(commits: &[CommitLogEntry]) -> Vec<GraphRow> {
    let mut active: Vec<Option<String>> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());

    for commit in commits {
        let my_lane = match active.iter().position(|slot| slot.as_deref() == Some(commit.hash.as_str())) {
            Some(lane) => lane,
            None => match active.iter().position(|slot| slot.is_none()) {
                Some(lane) => lane,
                None => {
                    active.push(None);
                    active.len() - 1
                }
            },
        };

        let mut edges = Vec::new();
        let mut new_active = active.clone();

        // Any other lane also waiting on this exact commit converges here --
        // two branches that turn out to share this ancestor with no separate
        // merge commit recording it.
        for (lane, slot) in active.iter().enumerate() {
            if lane != my_lane && slot.as_deref() == Some(commit.hash.as_str()) {
                edges.push((lane, my_lane));
                new_active[lane] = None;
            }
        }

        match commit.parents.split_first() {
            Some((first, rest)) => {
                new_active[my_lane] = Some(first.clone());
                edges.push((my_lane, my_lane));
                for parent in rest {
                    let lane = match new_active.iter().position(|slot| slot.as_deref() == Some(parent.as_str()))
                    {
                        Some(lane) => lane,
                        None => match new_active.iter().position(|slot| slot.is_none()) {
                            Some(lane) => lane,
                            None => {
                                new_active.push(None);
                                new_active.len() - 1
                            }
                        },
                    };
                    new_active[lane] = Some(parent.clone());
                    edges.push((my_lane, lane));
                }
            }
            // A root commit: this lane has nothing left to continue to.
            None => new_active[my_lane] = None,
        }

        // Every lane that was active before this row and is neither `my_lane`
        // nor a merge target touched above passes straight through.
        for (lane, slot) in active.iter().enumerate() {
            if lane == my_lane || edges.iter().any(|&(from, _)| from == lane) {
                continue;
            }
            if slot.is_some() {
                edges.push((lane, lane));
            }
        }

        rows.push(GraphRow { lane: my_lane, edges });
        active = new_active;
    }

    rows
}

/// The widest lane index reached across every row, so a renderer can size
/// the graph column before it draws anything.
pub fn lane_count(rows: &[GraphRow]) -> usize {
    rows.iter()
        .flat_map(|row| row.edges.iter().flat_map(|&(a, b)| [a, b]).chain(std::iter::once(row.lane)))
        .max()
        .map(|max| max + 1)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(hash: &str, parents: &[&str]) -> CommitLogEntry {
        CommitLogEntry {
            hash: hash.to_string(),
            short_hash: hash.to_string(),
            author: "A".to_string(),
            date: "2026-01-01".to_string(),
            summary: hash.to_string(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn an_empty_log_has_no_rows() {
        assert_eq!(lanes(&[]), Vec::new());
    }

    #[test]
    fn a_straight_line_of_single_parent_commits_all_stay_in_lane_zero() {
        let log = vec![commit("c3", &["c2"]), commit("c2", &["c1"]), commit("c1", &[])];
        let rows = lanes(&log);
        assert_eq!(rows.iter().map(|r| r.lane).collect::<Vec<_>>(), vec![0, 0, 0]);
        // Each row continues straight into the next -- no diagonal anywhere
        // in a history with no branching at all.
        for row in &rows {
            assert!(row.edges.iter().all(|&(a, b)| a == b), "unexpected fork: {row:?}");
        }
    }

    #[test]
    fn a_root_commit_terminates_its_lane() {
        let log = vec![commit("c1", &[])];
        let rows = lanes(&log);
        assert_eq!(rows[0].lane, 0);
        assert_eq!(rows[0].edges, Vec::new(), "no parent means no edge leaving this row");
    }

    #[test]
    fn a_merge_commit_opens_a_second_lane_for_its_second_parent() {
        // m merges "feature" (parent f) into "main" (parent c1); f and c1
        // both eventually reach base, the common ancestor.
        let log = vec![
            commit("m", &["c1", "f"]),
            commit("f", &["base"]),
            commit("c1", &["base"]),
            commit("base", &[]),
        ];
        let rows = lanes(&log);
        // m: first parent (c1) continues in m's own lane 0; second parent
        // (f) forks a new lane.
        assert_eq!(rows[0].lane, 0);
        assert!(rows[0].edges.contains(&(0, 0)), "first parent stays in lane 0: {:?}", rows[0].edges);
        assert!(rows[0].edges.iter().any(|&(a, b)| a == 0 && b != 0), "second parent forks: {:?}", rows[0].edges);
        let fork_lane = rows[0].edges.iter().find(|&&(a, b)| a == 0 && b != 0).unwrap().1;

        // f (the row right after) sits in the forked lane.
        assert_eq!(rows[1].lane, fork_lane);

        // c1 sits back in lane 0, m's own continuation.
        assert_eq!(rows[2].lane, 0);

        // base is what both f and c1 point to -- the two lanes converge on
        // its row (one diagonal edge, the other lane folding into this
        // one), and base itself is a root, so nothing continues past it.
        assert_eq!(rows[3].edges.len(), 1, "exactly the convergence, base itself is a root: {:?}", rows[3]);
        let base_lane = rows[3].lane;
        assert!(base_lane == 0 || base_lane == fork_lane);
    }

    #[test]
    fn two_still_open_tips_do_not_collide_in_one_lane() {
        // Both tips have a parent still pending (not yet in the log), so
        // both lanes stay reserved and must land in different columns --
        // unlike two *root* commits, whose lanes free up immediately and are
        // legitimately available for reuse.
        let log = vec![commit("a", &["pending-a"]), commit("b", &["pending-b"])];
        let rows = lanes(&log);
        assert_ne!(rows[0].lane, rows[1].lane, "two unrelated open tips must not collide in one lane");
    }

    #[test]
    fn a_roots_freed_lane_is_available_to_the_very_next_unrelated_tip() {
        // Once "a" (a root) is drawn, lane 0 has nothing left to continue --
        // exactly the case a real graph reuses that column for, rather than
        // growing sideways forever.
        let log = vec![commit("a", &[]), commit("b", &[])];
        let rows = lanes(&log);
        assert_eq!(rows[0].lane, 0);
        assert_eq!(rows[1].lane, 0, "b reuses a's freed lane rather than opening a new column");
    }

    #[test]
    fn lane_count_reports_the_widest_column_actually_used() {
        let log = vec![
            commit("m", &["c1", "f"]),
            commit("f", &["base"]),
            commit("c1", &["base"]),
            commit("base", &[]),
        ];
        let rows = lanes(&log);
        assert_eq!(lane_count(&rows), 2, "a single two-way merge never needs a third lane");
    }
}
