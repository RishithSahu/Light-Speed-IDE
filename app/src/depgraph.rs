//! The dependency view: `force_graph` places the nodes, this draws them.
//!
//! # The split
//!
//! Deciding where a hundred related circles should sit so that the picture
//! reads is a solved problem with a literature behind it, and re-deriving it
//! here would have been the wrong work. `force_graph` runs the simulation --
//! every node pushing every other apart, every edge pulling its two ends
//! together, until the arrangement settles. It returns coordinates and
//! nothing else, which is the whole of what is wanted from it: the circles,
//! arrows, colours and labels below are this renderer's own, drawn from the
//! same quads and glyph runs as the rest of the window.
//!
//! ```text
//!   ls_core::dependency_graph        force_graph            this module
//!   (which file imports which)  ->  (where each node  ->  (quads + text the
//!                                    settles)              renderer draws)
//! ```
//!
//! # Three stages, because only one of them is expensive
//!
//! [`Settled`] is the simulation's answer: who is connected to whom and where
//! each file came to rest. It costs a few hundred milliseconds, depends on
//! nothing but the code, and so is what gets [cached](encode) between
//! sessions.
//!
//! [`Scene`] fits that answer to a pane, sizing the circles to the space
//! actually available. It is cheap, and re-runs whenever the window resizes.
//!
//! [`View`] is where the reader has panned and zoomed to. It is applied at
//! draw time and touches neither of the other two, so dragging the graph
//! around never re-runs a simulation.
//!
//! # Why a force layout and not a layered one
//!
//! The first version used a layered (Sugiyama) layout, which is the
//! canonical choice for a dependency graph and the wrong one here. Layering
//! ranks nodes by depth, and a codebase is shallow and wide: seventy-nine
//! files fell into a handful of ranks and laid out at 12218x210 px -- eleven
//! screens across. Turned on its side it became 1798x5732, and either way
//! only about seven files were legible at once. A force layout has no ranks
//! to spread along, so it packs the same graph into a round cluster sized to
//! whatever pane it is given. Direction is carried by the arrowheads rather
//! than by position.
//!
//! # Determinism
//!
//! The simulation is seeded from a fixed spiral, never from a random number
//! generator, so the same repository always produces the same picture -- both
//! across runs and against whatever was cached last time.
//!
//! # Labels ride the character grid
//!
//! The text engine shapes one buffer per named region, each drawn at a
//! single origin, so it cannot place a hundred labels at a hundred arbitrary
//! points. Rather than build a second text path for one view, labels are
//! stamped into a monospace character grid ([`label_rows`]) and handed over
//! as ordinary region text. They keep their size as the graph zooms, the way
//! a map's place names do; what zooming buys is room for more of them, and
//! more of each name.

use crate::layout::Rect;
use crate::quads::Quad;
use crate::theme::Theme;
use force_graph::{EdgeData, ForceGraph, NodeData, SimulationParameters};
use ls_core::dependency_graph::DependencyGraph;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How thick an edge is drawn, in logical pixels.
const EDGE_THICKNESS: f32 = 1.0;
/// How thick the edges into the node under the pointer are drawn.
const TRACED_THICKNESS: f32 = 2.0;
/// Length of each stroke of an arrowhead.
const ARROWHEAD: f32 = 7.0;
/// Smallest a circle may be drawn, however tight the graph.
const MIN_RADIUS: f32 = 5.0;
/// Largest a circle may be drawn, however loose it.
const MAX_RADIUS: f32 = 46.0;
/// A circle's radius as a share of the distance to its nearest neighbour.
///
/// This is what keeps the picture from collapsing. Circles were originally a
/// fixed size while positions were scaled to fit the pane, and connected
/// nodes -- which a force layout deliberately pulls *together* -- ended up
/// overlapping, so `trim` refused to draw between them: 88% of this
/// repository's edges went missing. Sizing every circle from the spacing
/// actually achieved means a crowded graph gets small circles and a sparse
/// one gets large ones, and in both there is room for the lines.
const RADIUS_OF_SPACING: f32 = 0.34;
/// Which of the gaps the circles are sized to fit. Low, not average: the
/// tightest tenth of a graph is where circles collide and edges vanish.
const TIGHT_PERCENTILE: f32 = 0.1;
/// The smallest circle, as a share of the largest. Kept well below 1 so
/// degree is still readable at a glance, and well above 0 so a leaf is not a
/// speck.
const QUIET_SHARE: f32 = 0.45;
/// Degree at which a node reaches the largest radius its graph allows.
const BUSY_DEGREE: f32 = 12.0;
/// Gap left between the cluster and the edge of the pane.
const MARGIN: f32 = 24.0;
/// Longest filename drawn on a node at the closest zoom.
///
/// Deliberately not tied to the circle's own width, which was the first
/// attempt and made the view useless: even a hub is only about eleven
/// characters across, so `workspace_search.rs` came out as `workspac...` and
/// every file looked alike. A label wider than its circle overhangs it,
/// which reads perfectly well against the pane; names that would collide are
/// dropped by [`label_rows`], smallest node first.
const LABEL_CAP: usize = 28;
/// Shortest a label may be cut to before it is dropped instead. Three
/// characters of a filename say nothing worth the clutter.
const LABEL_FLOOR: usize = 5;
/// Simulation steps. Each moves a node only a little, so it takes a good
/// number of them to settle; see `real_repo::extent_of_this_repository` for
/// what it costs.
const STEPS: usize = 600;
/// Seconds per simulation step.
const STEP: f32 = 0.02;

/// How the simulation is driven.
///
/// `force_graph`'s own defaults are meant for animating a handful of nodes a
/// frame at a time, and they tear this apart: `node_speed` is 7000, and a
/// node here feels a force from all seventy-eight others at once, so a
/// single step moved nodes thousands of pixels. The simulation diverged --
/// settled positions spanned two million pixels, everything collapsed into a
/// dot when fitted, and 22 of 156 edges survived. These values were measured
/// rather than guessed; [`settle`] documents the arithmetic.
///
/// The charge and spring were then swept against how much of this repository
/// survives. Both matter to how *evenly* the graph spreads, and an uneven
/// one has to draw small circles to keep its tightest corner legible. The
/// crate's own 12000/0.3 gave circles of 6.5-14.4 px; loosening the spring
/// and raising the charge to 30000/0.15 gives 7.7-17.1 px with all 156 edges
/// still drawn. Going further -- 30000/0.10 -- reaches 10.2-22.6 px but
/// loses four edges to circles that grow into each other, and a missing
/// dependency is worse than a small circle.
fn parameters() -> SimulationParameters {
    SimulationParameters {
        force_charge: 30_000.0,
        force_spring: 0.15,
        force_max: 280.0,
        node_speed: 100.0,
        damping_factor: 0.9,
    }
}

// --- stage 1: the settled simulation (expensive, cached) ---------------------

/// Where the simulation left every file, and what joins them.
///
/// Everything here depends only on the code that was scanned -- not on the
/// window, the theme, or where the reader has panned to -- which is what
/// makes it worth keeping on disk between sessions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settled {
    /// Workspace-relative paths, in the order everything else indexes them.
    pub files: Vec<PathBuf>,
    /// `(importer, imported)` pairs, as indices into `files`.
    pub edges: Vec<(usize, usize)>,
    /// Where each file came to rest, in the simulation's own coordinates.
    pub positions: Vec<(f32, f32)>,
    /// Whether the scan stopped early because the workspace was too large.
    pub truncated: bool,
}

/// Runs the simulation over a scanned graph.
pub fn settle_graph(graph: &DependencyGraph) -> Settled {
    let index: HashMap<&Path, usize> =
        graph.files.iter().enumerate().map(|(at, file)| (file.as_path(), at)).collect();

    let mut degree = vec![0.0_f32; graph.files.len()];
    let mut edges: Vec<(usize, usize)> = Vec::with_capacity(graph.edges.len());
    for edge in &graph.edges {
        let (Some(from), Some(to)) = (index.get(edge.from.as_path()), index.get(edge.to.as_path()))
        else {
            continue;
        };
        degree[*from] += 1.0;
        degree[*to] += 1.0;
        edges.push((*from, *to));
    }

    Settled {
        positions: settle(&degree, &edges),
        files: graph.files.clone(),
        edges,
        truncated: graph.truncated,
    }
}

/// Runs the simulation and returns where every node settled.
///
/// The step size and speed are bounded by how far one step may move a node.
/// The integrator is `v += a * dt * node_speed` and `x += v * dt`, and `a` is
/// the sum of every pair force, each clamped to `force_max`. With n nodes
/// that is at worst `n * force_max * dt`, so one step displaces a node by
/// about `n * force_max * node_speed * dt^3`. At n = 80 and the values in
/// [`parameters`] that is roughly 18 px -- small against the ~200 px the
/// nodes settle apart at, which is what keeps it stable.
fn settle(degree: &[f32], edges: &[(usize, usize)]) -> Vec<(f32, f32)> {
    if degree.is_empty() {
        return Vec::new();
    }
    let mut simulation: ForceGraph<(), ()> = ForceGraph::new(parameters());
    let handles: Vec<_> = degree
        .iter()
        .enumerate()
        .map(|(at, degree)| {
            let (x, y) = seed(at);
            simulation.add_node(NodeData {
                x,
                y,
                // A busy file shoulders a little more room. Kept mild:
                // repulsion goes as the product of the two masses, so a wide
                // spread of masses makes the hubs shove hard enough to
                // destabilise the whole simulation.
                mass: 10.0 + degree * 0.5,
                is_anchor: false,
                user_data: (),
            })
        })
        .collect();
    for (from, to) in edges {
        simulation.add_edge(handles[*from], handles[*to], EdgeData::default());
    }
    for _ in 0..STEPS {
        simulation.update(STEP);
    }

    let mut settled = Vec::with_capacity(degree.len());
    simulation.visit_nodes(|node| settled.push((node.x(), node.y())));
    settled
}

/// A node's starting position, on a golden-angle spiral.
///
/// Fixed rather than random, for two reasons. The picture has to be the same
/// every time the view is opened, or nothing in it can be recognised twice
/// and a cached layout would not match a fresh one. And no two nodes may
/// start on the same point: coincident nodes feel no direction to separate
/// along and stay welded together for the whole run.
fn seed(at: usize) -> (f32, f32) {
    const GOLDEN_ANGLE: f32 = 2.399_963_2;
    let angle = at as f32 * GOLDEN_ANGLE;
    let radius = 30.0 * (at as f32 + 1.0).sqrt();
    (radius * angle.cos(), radius * angle.sin())
}

// --- the cache format --------------------------------------------------------

/// Version marker on the first line of a cache file. Bumped whenever the
/// format changes, so an old file is ignored rather than misread.
const CACHE_VERSION: &str = "lightspeed-depgraph 1";

/// Writes a settled graph out as text.
///
/// A plain line-based format, not JSON: the whole file is a version line, a
/// root line, and then two runs of fixed-shape rows, which takes a dozen
/// lines to write and a dozen to read. The workspace root goes in so a cache
/// can prove it belongs to the folder being opened.
///
/// Paths come last on their line because a path may contain spaces and
/// nothing else may.
pub fn encode(root: &Path, settled: &Settled) -> String {
    let mut out = String::with_capacity(settled.files.len() * 48);
    out.push_str(CACHE_VERSION);
    out.push('\n');
    out.push_str(&format!("root {}\n", root.display()));
    out.push_str(&format!("truncated {}\n", settled.truncated));
    out.push_str(&format!("files {}\n", settled.files.len()));
    for (at, file) in settled.files.iter().enumerate() {
        let (x, y) = settled.positions.get(at).copied().unwrap_or((0.0, 0.0));
        out.push_str(&format!("{x:.3} {y:.3} {}\n", file.display()));
    }
    out.push_str(&format!("edges {}\n", settled.edges.len()));
    for (from, to) in &settled.edges {
        out.push_str(&format!("{from} {to}\n"));
    }
    out
}

/// Reads a settled graph back, or `None` if the text is not one this version
/// wrote for this folder.
///
/// Every failure is a `None` rather than an error: a cache that cannot be
/// read means a rescan, which is exactly what happens on a first visit, so
/// there is nothing for a caller to handle differently. That covers a bumped
/// version, a truncated file, a folder that was renamed, and an edge whose
/// index no longer points at a file.
pub fn decode(root: &Path, text: &str) -> Option<Settled> {
    let mut lines = text.lines();
    if lines.next()? != CACHE_VERSION {
        return None;
    }
    if lines.next()?.strip_prefix("root ")? != root.display().to_string() {
        return None;
    }
    let truncated = lines.next()?.strip_prefix("truncated ")? == "true";

    let count: usize = lines.next()?.strip_prefix("files ")?.parse().ok()?;
    let mut files = Vec::with_capacity(count);
    let mut positions = Vec::with_capacity(count);
    for _ in 0..count {
        let line = lines.next()?;
        let (x, rest) = line.split_once(' ')?;
        let (y, path) = rest.split_once(' ')?;
        let point = (x.parse::<f32>().ok()?, y.parse::<f32>().ok()?);
        if !point.0.is_finite() || !point.1.is_finite() {
            return None;
        }
        positions.push(point);
        files.push(PathBuf::from(path));
    }

    let count: usize = lines.next()?.strip_prefix("edges ")?.parse().ok()?;
    let mut edges = Vec::with_capacity(count);
    for _ in 0..count {
        let (from, to) = lines.next()?.split_once(' ')?;
        let edge = (from.parse::<usize>().ok()?, to.parse::<usize>().ok()?);
        if edge.0 >= files.len() || edge.1 >= files.len() {
            return None;
        }
        edges.push(edge);
    }

    Some(Settled { files, edges, positions, truncated })
}

// --- stage 2: fitting the settled graph to a pane ----------------------------

/// One straight stroke of an edge, and which edge it belongs to.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Segment {
    pub from: (f32, f32),
    pub to: (f32, f32),
    /// Index into [`Scene::edges`], so the edges touching the node under the
    /// pointer can be picked out.
    pub edge: usize,
}

/// One file, as drawn.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Centre in scene coordinates, before the reader's pan and zoom.
    pub centre: (f32, f32),
    pub radius: f32,
    /// Workspace-relative path, which is what a click on this node opens.
    pub path: PathBuf,
    /// The file's own name, uncut -- [`label_rows`] decides how much of it
    /// there is room to draw.
    pub name: String,
    /// Whether anything in the workspace imports this file. What nothing
    /// imports is an entry point, and gets the second colour.
    pub imported: bool,
}

/// A graph fitted to a pane. Colours are not baked in: the renderer picks
/// them from the theme, so changing theme costs a redraw and not a relayout.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    /// Biggest first, so a small node drawn later sits on top of a hub
    /// rather than disappearing behind it.
    pub nodes: Vec<Node>,
    pub segments: Vec<Segment>,
    /// `(importer, imported)` as indices into `nodes`.
    pub edges: Vec<(usize, usize)>,
    /// The pane this was fitted to.
    pub width: f32,
    pub height: f32,
    /// The box the circles actually occupy, rims included, in scene
    /// coordinates. Distinct from `width`/`height`, which is the whole pane:
    /// a two-file graph sits in a small patch in the middle of it, and
    /// clamping the pan against the pane rather than against this let such a
    /// graph be flung off screen and lost.
    pub bounds: (f32, f32, f32, f32),
}

impl Scene {
    /// Whether `node` is either end of `edge`.
    pub fn touches(&self, edge: usize, node: usize) -> bool {
        self.edges.get(edge).is_some_and(|(from, to)| *from == node || *to == node)
    }

    /// How many files this one imports, and how many import it.
    pub fn connections(&self, node: usize) -> (usize, usize) {
        let out = self.edges.iter().filter(|(from, _)| *from == node).count();
        let inward = self.edges.iter().filter(|(_, to)| *to == node).count();
        (out, inward)
    }
}

/// Fits a settled graph to a `viewport`-sized pane.
///
/// The viewport is an input rather than something applied afterwards because
/// the layout is *fitted* to it: the cluster is normalised into the pane so
/// the whole repository is visible at once before the reader touches
/// anything. Circles are then sized from the spacing that fit actually
/// achieved.
pub fn build_scene(settled: &Settled, viewport: (f32, f32)) -> Scene {
    if settled.files.is_empty() {
        return Scene::default();
    }

    let mut degree = vec![0.0_f32; settled.files.len()];
    let mut imported = vec![false; settled.files.len()];
    for (from, to) in &settled.edges {
        degree[*from] += 1.0;
        degree[*to] += 1.0;
        imported[*to] = true;
    }

    // Fitted twice: the first pass reserves room for the biggest circle a
    // node could possibly want, which is what says how much space each node
    // actually got, which is what decides how big the circles really are.
    // The second pass then reclaims whatever the first over-reserved.
    let roomy = fit(&settled.positions, MAX_RADIUS, viewport);
    let cap =
        (crowding(&roomy, &settled.edges) * RADIUS_OF_SPACING).clamp(MIN_RADIUS, MAX_RADIUS);
    let placed = fit(&settled.positions, cap, viewport);
    let radii: Vec<f32> = degree.iter().map(|degree| radius_for(*degree, cap)).collect();

    // Biggest first: a hub drawn last would swallow the small nodes near it,
    // and `label_rows` gives the earlier label the grid cells it wants.
    let mut order: Vec<usize> = (0..settled.files.len()).collect();
    order.sort_by(|a, b| radii[*b].total_cmp(&radii[*a]).then(a.cmp(b)));
    let mut drawn_at = vec![0usize; settled.files.len()];
    for (position, original) in order.iter().enumerate() {
        drawn_at[*original] = position;
    }

    let nodes: Vec<Node> = order
        .iter()
        .map(|at| Node {
            centre: placed[*at],
            radius: radii[*at],
            path: settled.files[*at].clone(),
            name: settled.files[*at]
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            imported: imported[*at],
        })
        .collect();

    let edges: Vec<(usize, usize)> =
        settled.edges.iter().map(|(from, to)| (drawn_at[*from], drawn_at[*to])).collect();

    let mut segments = Vec::with_capacity(edges.len() * 3);
    for (at, (from, to)) in edges.iter().enumerate() {
        let (start, end) = (nodes[*from].centre, nodes[*to].centre);
        // Trimmed to the rims, so the arrowhead lands against the circle it
        // points at rather than underneath it.
        let Some((tail, tip)) = trim(start, nodes[*from].radius, end, nodes[*to].radius) else {
            continue;
        };
        segments.push(Segment { from: tail, to: tip, edge: at });
        push_arrowhead(&mut segments, tail, tip, at);
    }

    let mut bounds = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for node in &nodes {
        bounds.0 = bounds.0.min(node.centre.0 - node.radius);
        bounds.1 = bounds.1.min(node.centre.1 - node.radius);
        bounds.2 = bounds.2.max(node.centre.0 + node.radius);
        bounds.3 = bounds.3.max(node.centre.1 + node.radius);
    }

    Scene { nodes, segments, edges, width: viewport.0, height: viewport.1, bounds }
}

/// The radius a node of this degree is drawn at, given the largest radius
/// this graph's spacing allows.
fn radius_for(degree: f32, cap: f32) -> f32 {
    let share = (degree / BUSY_DEGREE).clamp(0.0, 1.0);
    // Square-rooted so the first few connections make a visible difference
    // and the twentieth does not have to make the circle enormous.
    cap * (QUIET_SHARE + (1.0 - QUIET_SHARE) * share.sqrt())
}

/// The distance the circles have to fit inside: a low percentile of the gaps
/// that matter, so most of them clear rather than merely half.
///
/// Measured across the *edges* where there are any, because those are the
/// pairs whose line has to be drawable, and a force layout deliberately
/// pulls them closer than anything else. The first version measured every
/// node's nearest neighbour and took the median, which by definition leaves
/// half the graph tighter than the answer -- on this repository that drew 22
/// of 156 edges. Falls back to nearest neighbours for a graph with no edges
/// at all, which has no lines to lose.
fn crowding(positions: &[(f32, f32)], edges: &[(usize, usize)]) -> f32 {
    let distance = |a: usize, b: usize| {
        let (dx, dy) = (positions[b].0 - positions[a].0, positions[b].1 - positions[a].1);
        (dx * dx + dy * dy).sqrt()
    };
    if positions.len() < 2 {
        return MAX_RADIUS * 2.0;
    }

    let mut gaps: Vec<f32> = if edges.is_empty() {
        (0..positions.len())
            .map(|at| {
                (0..positions.len())
                    .filter(|other| *other != at)
                    .map(|other| distance(at, other))
                    .fold(f32::MAX, f32::min)
            })
            .collect()
    } else {
        edges.iter().map(|(from, to)| distance(*from, *to)).collect()
    };
    gaps.sort_by(f32::total_cmp);
    gaps[((gaps.len() as f32 * TIGHT_PERCENTILE) as usize).min(gaps.len() - 1)]
}

/// Moves and scales the settled cluster to fill `viewport`, keeping its
/// proportions so the arrangement is not stretched into a smear.
///
/// `biggest_radius` is the room to leave around the outermost nodes so their
/// circles are not clipped by the pane's edge.
fn fit(positions: &[(f32, f32)], biggest_radius: f32, viewport: (f32, f32)) -> Vec<(f32, f32)> {
    let inset = MARGIN + biggest_radius;
    let width = (viewport.0 - inset * 2.0).max(1.0);
    let height = (viewport.1 - inset * 2.0).max(1.0);

    let mut bounds = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for (x, y) in positions {
        bounds.0 = bounds.0.min(*x);
        bounds.1 = bounds.1.min(*y);
        bounds.2 = bounds.2.max(*x);
        bounds.3 = bounds.3.max(*y);
    }
    let spread = ((bounds.2 - bounds.0).max(0.001), (bounds.3 - bounds.1).max(0.001));
    // One scale for both axes, and never magnified past life size: blowing a
    // three-file graph up to fill a window would say something about it that
    // is not true.
    let scale = (width / spread.0).min(height / spread.1).min(1.0);

    let used = (spread.0 * scale, spread.1 * scale);
    let origin = (inset + (width - used.0) / 2.0, inset + (height - used.1) / 2.0);

    positions
        .iter()
        .map(|(x, y)| (origin.0 + (x - bounds.0) * scale, origin.1 + (y - bounds.1) * scale))
        .collect()
}

/// Shortens an edge to run rim to rim between two circles, or `None` when
/// they sit so close that there is no line left to draw.
fn trim(
    from: (f32, f32),
    from_radius: f32,
    to: (f32, f32),
    to_radius: f32,
) -> Option<((f32, f32), (f32, f32))> {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let length = (dx * dx + dy * dy).sqrt();
    if length <= from_radius + to_radius + 1.0 {
        return None;
    }
    let (ux, uy) = (dx / length, dy / length);
    Some((
        (from.0 + ux * from_radius, from.1 + uy * from_radius),
        (to.0 - ux * to_radius, to.1 - uy * to_radius),
    ))
}

/// Two short strokes back from `tip`, angled off the incoming direction.
fn push_arrowhead(into: &mut Vec<Segment>, from: (f32, f32), tip: (f32, f32), edge: usize) {
    let (dx, dy) = (tip.0 - from.0, tip.1 - from.1);
    if (dx * dx + dy * dy).sqrt() < 0.001 {
        return;
    }
    let angle = dy.atan2(dx);
    for spread in [2.6_f32, -2.6] {
        let barb = angle + spread;
        into.push(Segment {
            from: tip,
            to: (tip.0 + ARROWHEAD * barb.cos(), tip.1 + ARROWHEAD * barb.sin()),
            edge,
        });
    }
}

// --- stage 3: where the reader has panned and zoomed to ----------------------

/// Closest the graph may be zoomed in.
pub const MAX_ZOOM: f32 = 8.0;
/// Furthest it may be zoomed out. Below this the scene is smaller than the
/// pane it was already fitted to, which shows nothing new.
pub const MIN_ZOOM: f32 = 0.4;
/// How far inside the pane the middle of the graph is kept, so it can never
/// be dragged away and lost. Comfortably more than the largest circle, so
/// whatever sits at the middle is drawn whole.
const MUST_STAY_VISIBLE: f32 = 100.0;

/// A pan and zoom over the scene.
///
/// Applied at draw time only. Nothing here feeds back into [`Scene`] or
/// [`Settled`], which is what makes dragging free.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct View {
    pub zoom: f32,
    pub pan: (f32, f32),
}

impl Default for View {
    /// The whole graph, as [`build_scene`] fitted it to the pane.
    fn default() -> Self {
        View { zoom: 1.0, pan: (0.0, 0.0) }
    }
}

impl View {
    /// Where a scene point lands on screen.
    pub fn to_screen(self, point: (f32, f32), pane: Rect) -> (f32, f32) {
        (pane.x + point.0 * self.zoom + self.pan.0, pane.y + point.1 * self.zoom + self.pan.1)
    }

    /// Which scene point is under a screen position.
    pub fn to_scene(self, point: (f32, f32), pane: Rect) -> (f32, f32) {
        ((point.0 - pane.x - self.pan.0) / self.zoom, (point.1 - pane.y - self.pan.1) / self.zoom)
    }

    /// Zooms by `factor` about `pointer`, so whatever is under the pointer
    /// stays under it -- the behaviour that makes a wheel feel attached to
    /// the picture rather than to the window.
    pub fn zoomed_at(self, factor: f32, pointer: (f32, f32), pane: Rect) -> View {
        let anchor = self.to_scene(pointer, pane);
        let zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        View {
            zoom,
            pan: (
                pointer.0 - pane.x - anchor.0 * zoom,
                pointer.1 - pane.y - anchor.1 * zoom,
            ),
        }
    }

    /// Drags the graph by a screen distance.
    pub fn panned(self, by: (f32, f32)) -> View {
        View { pan: (self.pan.0 + by.0, self.pan.1 + by.1), ..self }
    }

    /// Pulls the pan back until the graph is on screen again, so a hard
    /// flick of the mouse can never leave the reader staring at an empty
    /// pane with no way back.
    ///
    /// What is kept on screen is the *middle* of the circles -- not the
    /// pane the scene was fitted to, and not merely a sliver of the
    /// bounding box. Both weaker rules were tried and both lost the graph: a
    /// small graph occupies a patch in the middle of its pane, so guarding
    /// the pane guards nothing; and eighty pixels of bounding box can be
    /// eighty pixels of empty space between circles, which draws nothing
    /// because a circle only half inside the pane is dropped rather than
    /// clipped. The middle of a force layout is where its nodes are.
    pub fn clamped(self, scene: &Scene, pane: Rect) -> View {
        if scene.nodes.is_empty() {
            return self;
        }
        let (left, top, right, bottom) = scene.bounds;
        let axis = |low: f32, high: f32, extent: f32, pan: f32| {
            let middle = (low + high) / 2.0 * self.zoom;
            // Inset far enough that a circle sitting at the middle is whole
            // rather than straddling the border.
            let inset = MUST_STAY_VISIBLE.min(extent / 2.0);
            let lowest = inset - middle;
            let highest = extent - inset - middle;
            pan.clamp(lowest.min(highest), highest.max(lowest))
        };
        View {
            pan: (
                axis(left, right, pane.width, self.pan.0),
                axis(top, bottom, pane.height, self.pan.1),
            ),
            ..self
        }
    }
}

/// How far the pointer may travel between press and release and still count
/// as a click rather than a drag.
const CLICK_SLOP: f32 = 4.0;

/// Whether a press that went down at `origin` and came up at `release` was a
/// click on a node, or a drag of the canvas.
///
/// One button does both, so this is the whole of what separates them, and it
/// has to be measured from where the press *started*. A first version
/// compared against the previous mouse-move instead -- the drag handler
/// advanced its own anchor every frame -- so at release the pointer had
/// always "just" moved zero pixels and every drag ended by opening a file.
pub fn is_click(origin: (f32, f32), release: (f32, f32)) -> bool {
    (release.0 - origin.0).abs() <= CLICK_SLOP && (release.1 - origin.1).abs() <= CLICK_SLOP
}

/// The node under a screen position, if any.
///
/// Searched in reverse draw order so the answer is whichever circle the
/// reader can actually see: the scene lists hubs first, and the small nodes
/// drawn over them are the ones on top.
pub fn hit_test(scene: &Scene, pane: Rect, view: View, at: (f32, f32)) -> Option<usize> {
    if !pane.contains(at.0, at.1) {
        return None;
    }
    scene.nodes.iter().enumerate().rev().find(|(_, node)| {
        let centre = view.to_screen(node.centre, pane);
        let (dx, dy) = (at.0 - centre.0, at.1 - centre.1);
        let radius = (node.radius * view.zoom).max(MIN_RADIUS);
        dx * dx + dy * dy <= radius * radius
    })
    .map(|(at, _)| at)
}

// --- drawing -----------------------------------------------------------------

/// Turns the scene into quads under the reader's view.
///
/// Nodes are dropped when the pane does not fully contain them; edges are
/// clipped to it instead. The difference is forced by the renderer, which
/// has no scissor of its own: a circle drawn half outside the pane would
/// paint over the tab bar, but a line can simply be shortened to where it
/// crosses the border. Dropping edges the same way was tried first and left
/// the view as stubs -- an edge normally runs most of the way across the
/// graph, so nearly every one of them left the pane somewhere.
///
/// `traced` is the node under the pointer: it and everything it touches are
/// drawn brighter and thicker, which is how a reader follows one file's
/// dependencies out of a hundred crossing lines.
pub fn pane_quads(
    scene: &Scene,
    pane: Rect,
    view: View,
    theme: &Theme,
    traced: Option<usize>,
) -> Vec<Quad> {
    let mut quads: Vec<Quad> = Vec::with_capacity(scene.nodes.len() + scene.segments.len());

    for segment in &scene.segments {
        let lit = traced.is_some_and(|node| scene.touches(segment.edge, node));
        let from = view.to_screen(segment.from, pane);
        let to = view.to_screen(segment.to, pane);
        if let Some((from, to)) = clip_segment(from, to, pane) {
            let (colour, thickness) =
                if lit { (theme.text, TRACED_THICKNESS) } else { (theme.dim_text, EDGE_THICKNESS) };
            quads.push(Quad::line(from, to, thickness, colour));
        }
    }

    for (at, node) in scene.nodes.iter().enumerate() {
        let centre = view.to_screen(node.centre, pane);
        let radius = (node.radius * view.zoom).max(MIN_RADIUS);
        let colour = if node.imported { theme.cursor } else { theme.error };

        // The traced node gets a ring, drawn as a larger circle underneath
        // it -- the quad renderer fills shapes and cannot stroke one.
        if traced == Some(at) {
            let ring = radius + 3.0;
            if let Some(quad) = inside(pane, centre, ring, theme.text) {
                quads.push(quad);
            }
        }
        if let Some(quad) = inside(pane, centre, radius, colour) {
            quads.push(quad);
        }
    }
    quads
}

/// A circle quad, if the pane contains the whole of it.
fn inside(pane: Rect, centre: (f32, f32), radius: f32, colour: crate::theme::Color) -> Option<Quad> {
    let rect = Rect::new(centre.0 - radius, centre.1 - radius, radius * 2.0, radius * 2.0);
    let fits = rect.x >= pane.x
        && rect.y >= pane.y
        && rect.right() <= pane.right()
        && rect.bottom() <= pane.bottom();
    fits.then(|| Quad::ellipse(rect, colour))
}

/// Clips a line segment to `pane`, returning the part inside it, or `None`
/// when none of it is. Liang-Barsky: for each of the four borders, the
/// parameter range along the segment that stays inside is intersected with
/// what is left, and an empty result means the segment misses the pane.
fn clip_segment(from: (f32, f32), to: (f32, f32), pane: Rect) -> Option<((f32, f32), (f32, f32))> {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let (mut enter, mut leave) = (0.0_f32, 1.0_f32);

    for (direction, distance) in [
        (-dx, from.0 - pane.x),
        (dx, pane.right() - from.0),
        (-dy, from.1 - pane.y),
        (dy, pane.bottom() - from.1),
    ] {
        if direction == 0.0 {
            // Parallel to this border: either wholly inside it or wholly out.
            if distance < 0.0 {
                return None;
            }
            continue;
        }
        let crossing = distance / direction;
        if direction < 0.0 {
            enter = enter.max(crossing);
        } else {
            leave = leave.min(crossing);
        }
        if enter > leave {
            return None;
        }
    }

    let at = |t: f32| (from.0 + dx * t, from.1 + dy * t);
    Some((at(enter), at(leave)))
}

/// The font measurements the label grid is stamped on.
#[derive(Copy, Clone, Debug)]
pub struct GridMetrics {
    pub digit_width: f32,
    pub line_height: f32,
}

/// A stamped grid of labels, and where its first cell is drawn.
///
/// The origin is not the pane's: it slides with the pan, which is what stops
/// the labels shivering. See [`label_rows`].
#[derive(Clone, Debug, PartialEq)]
pub struct LabelGrid {
    pub text: String,
    pub origin: (f32, f32),
}

/// Stamps the labels into a monospace character grid covering `pane`.
///
/// # Why the grid slides
///
/// Each label is quantised to a whole character cell, because the text
/// engine draws one buffer per region from one origin and cannot place a
/// hundred names at a hundred arbitrary points. Quantising against the pane
/// made the labels unreadable the moment the graph moved: every name rounds
/// to its cell at a slightly different sub-pixel moment, so during a drag
/// they each jumped a cell at a different time and the whole picture
/// shivered.
///
/// So the grid is pinned to the *graph* rather than to the pane. Its origin
/// carries the pan's remainder within one cell, and each label's cell index
/// absorbs the whole cells:
///
/// ```text
///   drawn_x = origin.x + column * digit
///           = pane.x + pan.x + digit * round(scene_x * zoom / digit)
/// ```
///
/// The pan drops out of the rounding entirely, so a label's offset from its
/// own circle depends only on where that circle sits in the scene. It is
/// fixed while the reader drags, and every name travels with the graph
/// exactly as the circles do.
///
/// Labels keep their size as the graph zooms; what zooming buys is room --
/// more names fit without colliding, and each gets more of its own
/// characters. Where two want the same cells the earlier one keeps them, and
/// the scene lists nodes biggest first, so the hub keeps its name and the
/// small node beside it goes unlabelled. That is better than interleaving
/// two filenames into a third that does not exist.
pub fn label_rows(
    scene: &Scene,
    pane: Rect,
    view: View,
    metrics: GridMetrics,
    traced: Option<usize>,
) -> LabelGrid {
    let digit = metrics.digit_width.max(1.0);
    let line = metrics.line_height.max(1.0);

    // One cell of slack on each side, so a label straddling the pane's top
    // or left edge still has a cell to sit in rather than being dropped.
    let origin = (
        pane.x + view.pan.0.rem_euclid(digit) - digit,
        pane.y + view.pan.1.rem_euclid(line) - line,
    );
    let rows = (pane.height / line).ceil() as usize + 2;
    let columns = (pane.width / digit).ceil() as usize + 2;
    let mut grid: Vec<Vec<char>> = vec![Vec::new(); rows];

    // The traced node's name is drawn whatever else wanted those cells: the
    // reader is pointing at it, so it is the one label they asked for.
    let order = traced.into_iter().chain((0..scene.nodes.len()).filter(|at| Some(*at) != traced));

    for at in order {
        let node = &scene.nodes[at];
        let centre = view.to_screen(node.centre, pane);
        // Room grows with the circle, so zooming in fills names out.
        let radius = node.radius * view.zoom;
        let room = ((radius * 2.5) / digit).floor() as usize;
        let room = room.clamp(LABEL_FLOOR, LABEL_CAP);
        if room <= LABEL_FLOOR && node.name.chars().count() > LABEL_FLOOR * 2 {
            // Too little room to say anything useful; better blank.
            continue;
        }
        let text = cut(&node.name, room);
        let length = text.chars().count();
        if length == 0 {
            continue;
        }

        let row = ((centre.1 - origin.1) / line).round();
        if row < 0.0 || row as usize >= rows {
            continue;
        }
        let row = row as usize;
        let column = ((centre.0 - origin.0 - length as f32 * digit / 2.0) / digit).round();
        if column < 0.0 || column as usize + length > columns {
            continue;
        }
        let column = column as usize;

        let cells = &mut grid[row];
        if cells.len() < column + length {
            cells.resize(column + length, ' ');
        }
        // One space of clearance either side, so two labels sharing a row are
        // never run together into one unreadable word.
        let from = column.saturating_sub(1);
        let to = (column + length + 1).min(cells.len());
        if cells[from..to].iter().any(|cell| *cell != ' ') {
            continue;
        }
        for (offset, character) in text.chars().enumerate() {
            cells[column + offset] = character;
        }
    }

    LabelGrid {
        text: grid.iter().map(|cells| cells.iter().collect::<String>()).collect::<Vec<_>>().join("\n"),
        origin,
    }
}

/// A name cut to `room` characters, ellipsis included in the count.
fn cut(name: &str, room: usize) -> String {
    if name.chars().count() <= room {
        return name.to_string();
    }
    let mut short: String = name.chars().take(room.saturating_sub(1)).collect();
    short.push('\u{2026}');
    short
}

#[cfg(test)]
mod tests {
    use super::*;
    use ls_core::dependency_graph::Edge;

    const METRICS: GridMetrics = GridMetrics { digit_width: 8.0, line_height: 20.0 };
    const VIEWPORT: (f32, f32) = (1200.0, 700.0);
    const PANE: Rect = Rect::new(0.0, 0.0, 1200.0, 700.0);

    fn graph_of(files: &[&str], edges: &[(&str, &str)]) -> DependencyGraph {
        DependencyGraph {
            files: files.iter().map(PathBuf::from).collect(),
            edges: edges
                .iter()
                .map(|(from, to)| Edge { from: PathBuf::from(from), to: PathBuf::from(to) })
                .collect(),
            truncated: false,
        }
    }

    fn scene_of(files: &[&str], edges: &[(&str, &str)]) -> Scene {
        build_scene(&settle_graph(&graph_of(files, edges)), VIEWPORT)
    }

    fn circles(quads: &[Quad]) -> usize {
        quads.iter().filter(|quad| quad.shape == crate::quads::Shape::Ellipse).count()
    }

    #[test]
    fn a_graph_becomes_a_node_per_file_and_an_edge_per_import() {
        let scene = scene_of(
            &["main.rs", "parser.rs", "token.rs"],
            &[("main.rs", "parser.rs"), ("parser.rs", "token.rs")],
        );
        assert_eq!(scene.nodes.len(), 3);
        assert_eq!(scene.edges.len(), 2);
        assert!(scene.nodes.iter().any(|node| node.name == "main.rs"));
        assert_eq!(scene.segments.len(), 6, "a shaft and two barbs per edge");
    }

    #[test]
    fn the_whole_graph_lands_inside_the_pane_it_was_fitted_to() {
        // The point of fitting. The layered layout this replaced ran to
        // eleven screens wide.
        let files: Vec<String> = (0..60).map(|index| format!("file{index}.rs")).collect();
        let names: Vec<&str> = files.iter().map(String::as_str).collect();
        let edges: Vec<(&str, &str)> = (1..60).map(|index| (names[0], names[index])).collect();
        let scene = scene_of(&names, &edges);
        let quads = pane_quads(&scene, PANE, View::default(), &Theme::dark(), None);
        assert_eq!(circles(&quads), 60, "every one of the 60 files is on screen at once");
    }

    #[test]
    fn the_simulation_settles_instead_of_flying_apart() {
        // `force_graph`'s default `node_speed` of 7000 is meant for a few
        // nodes animated a frame at a time; with eighty all pushing at once
        // it moved them thousands of pixels per step. Positions ran to two
        // million pixels across, so fitting collapsed the graph into a dot.
        let files: Vec<String> = (0..80).map(|index| format!("file{index}.rs")).collect();
        let names: Vec<&str> = files.iter().map(String::as_str).collect();
        let edges: Vec<(&str, &str)> = (1..80).map(|index| (names[0], names[index])).collect();
        let settled = settle_graph(&graph_of(&names, &edges));

        for axis in [0, 1] {
            let values: Vec<f32> = settled
                .positions
                .iter()
                .map(|point| if axis == 0 { point.0 } else { point.1 })
                .collect();
            let reach = values.iter().copied().fold(f32::MIN, f32::max)
                - values.iter().copied().fold(f32::MAX, f32::min);
            assert!(reach.is_finite(), "the simulation produced a non-finite position");
            assert!(reach < 20_000.0, "the simulation diverged: nodes {reach:.0} px apart");
        }
        let scene = build_scene(&settled, VIEWPORT);
        assert_eq!(scene.segments.len() / 3, 79, "every edge is drawn");
    }

    #[test]
    fn a_crowded_graph_still_draws_its_edges() {
        // Circles used to be a fixed size while positions were squeezed to
        // fit, so the connected nodes a force layout pulls together
        // overlapped and their edges were dropped -- 88% of this
        // repository's went missing.
        let files: Vec<String> = (0..70).map(|index| format!("file{index}.rs")).collect();
        let names: Vec<&str> = files.iter().map(String::as_str).collect();
        let edges: Vec<(&str, &str)> =
            (1..70).map(|index| (names[index - 1], names[index])).collect();
        let scene = scene_of(&names, &edges);
        let drawn = scene.segments.len() / 3;
        assert!(drawn * 100 >= edges.len() * 90, "only {drawn} of {} survived", edges.len());
    }

    #[test]
    fn circles_stay_clear_of_their_neighbours() {
        let files: Vec<String> = (0..40).map(|index| format!("file{index}.rs")).collect();
        let names: Vec<&str> = files.iter().map(String::as_str).collect();
        let edges: Vec<(&str, &str)> = (1..40).map(|index| (names[0], names[index])).collect();
        let scene = scene_of(&names, &edges);

        let mut overlaps = 0;
        for (a, here) in scene.nodes.iter().enumerate() {
            for there in scene.nodes.iter().skip(a + 1) {
                let (dx, dy) = (there.centre.0 - here.centre.0, there.centre.1 - here.centre.1);
                if (dx * dx + dy * dy).sqrt() < here.radius + there.radius {
                    overlaps += 1;
                }
            }
        }
        assert_eq!(overlaps, 0, "{overlaps} pairs of circles overlap");
    }

    #[test]
    fn the_same_repository_lays_out_the_same_way_twice() {
        // Seeded from a spiral, not a random generator: a picture that
        // reshuffled itself could not be recognised, and a cached layout
        // would not match a fresh one.
        let files = ["a.rs", "b.rs", "c.rs", "d.rs"];
        let edges = [("a.rs", "b.rs"), ("b.rs", "c.rs"), ("c.rs", "d.rs")];
        assert_eq!(settle_graph(&graph_of(&files, &edges)), settle_graph(&graph_of(&files, &edges)));
    }

    #[test]
    fn nodes_never_start_stacked_on_one_another() {
        let mut seen: Vec<(f32, f32)> = Vec::new();
        for at in 0..200 {
            let point = seed(at);
            assert!(
                !seen.iter().any(|other| (other.0 - point.0).abs() < 0.001
                    && (other.1 - point.1).abs() < 0.001),
                "node {at} seeded on top of another"
            );
            seen.push(point);
        }
    }

    #[test]
    fn an_imported_file_and_an_entry_point_are_marked_differently() {
        let scene = scene_of(&["main.rs", "parser.rs"], &[("main.rs", "parser.rs")]);
        let entry = scene.nodes.iter().find(|node| node.name == "main.rs").expect("main.rs");
        let used = scene.nodes.iter().find(|node| node.name == "parser.rs").expect("parser.rs");
        assert!(!entry.imported, "nothing imports main.rs");
        assert!(used.imported);

        let theme = Theme::dark();
        let quads = pane_quads(&scene, PANE, View::default(), &theme, None);
        let colours: Vec<_> = quads
            .iter()
            .filter(|quad| quad.shape == crate::quads::Shape::Ellipse)
            .map(|quad| quad.color.srgb)
            .collect();
        assert!(colours.contains(&theme.error.srgb), "the entry point stands out");
        assert!(colours.contains(&theme.cursor.srgb));
    }

    #[test]
    fn a_busy_file_is_drawn_larger_than_a_quiet_one() {
        let mut names = vec!["hub.rs"];
        let leaves: Vec<String> = (0..10).map(|index| format!("leaf{index}.rs")).collect();
        names.extend(leaves.iter().map(String::as_str));
        let edges: Vec<(&str, &str)> =
            leaves.iter().map(|leaf| ("hub.rs", leaf.as_str())).collect();
        let scene = scene_of(&names, &edges);
        let hub = scene.nodes.iter().find(|node| node.name == "hub.rs").expect("hub");
        let leaf = scene.nodes.iter().find(|node| node.name == "leaf3.rs").expect("leaf");
        assert!(hub.radius > leaf.radius, "{} vs {}", hub.radius, leaf.radius);
    }

    #[test]
    fn an_edge_stops_at_the_rim_of_the_circle_it_points_at() {
        let (tail, tip) = trim((0.0, 0.0), 20.0, (200.0, 0.0), 30.0).expect("far enough apart");
        assert_eq!(tail, (20.0, 0.0));
        assert_eq!(tip, (170.0, 0.0));
    }

    #[test]
    fn two_circles_that_overlap_get_no_edge_drawn_between_them() {
        assert!(trim((0.0, 0.0), 20.0, (10.0, 0.0), 30.0).is_none());
    }

    #[test]
    fn an_empty_graph_produces_an_empty_scene_rather_than_panicking() {
        let scene = scene_of(&[], &[]);
        assert!(scene.nodes.is_empty());
        assert!(pane_quads(&scene, PANE, View::default(), &Theme::dark(), None).is_empty());
        assert_eq!(label_rows(&scene, PANE, View::default(), METRICS, None).text.trim(), "");
    }

    #[test]
    fn a_cycle_settles_instead_of_hanging() {
        let scene = scene_of(&["a.py", "b.py"], &[("a.py", "b.py"), ("b.py", "a.py")]);
        assert_eq!(scene.nodes.len(), 2);
        for node in &scene.nodes {
            assert!(node.centre.0.is_finite() && node.centre.1.is_finite());
        }
    }

    // --- view -----------------------------------------------------------

    #[test]
    fn zooming_keeps_whatever_is_under_the_pointer_under_it() {
        // What makes a wheel feel attached to the picture rather than to the
        // window.
        let pane = Rect::new(60.0, 100.0, 800.0, 600.0);
        let pointer = (300.0, 400.0);
        let view = View::default();
        let before = view.to_scene(pointer, pane);
        let after = view.zoomed_at(2.0, pointer, pane);
        let now = after.to_scene(pointer, pane);
        assert!((before.0 - now.0).abs() < 0.01, "{before:?} vs {now:?}");
        assert!((before.1 - now.1).abs() < 0.01, "{before:?} vs {now:?}");
        assert_eq!(after.zoom, 2.0);
    }

    #[test]
    fn zoom_stops_at_its_limits() {
        let pane = PANE;
        let mut view = View::default();
        for _ in 0..40 {
            view = view.zoomed_at(2.0, (600.0, 350.0), pane);
        }
        assert_eq!(view.zoom, MAX_ZOOM);
        for _ in 0..80 {
            view = view.zoomed_at(0.5, (600.0, 350.0), pane);
        }
        assert_eq!(view.zoom, MIN_ZOOM);
    }

    #[test]
    fn the_graph_cannot_be_dragged_off_screen_and_lost() {
        // A two-file graph sits in a small patch in the middle of the pane
        // it was fitted to. Clamping the pan against the pane rather than
        // against the circles let exactly this case be flung out of sight.
        for (dx, dy) in [(99_000.0, 99_000.0), (-99_000.0, -99_000.0), (99_000.0, -99_000.0)] {
            let scene = scene_of(&["a.rs", "b.rs"], &[("a.rs", "b.rs")]);
            let flung = View::default().panned((dx, dy)).clamped(&scene, PANE);
            let quads = pane_quads(&scene, PANE, flung, &Theme::dark(), None);
            assert!(!quads.is_empty(), "dragged ({dx}, {dy}) and lost: pan {:?}", flung.pan);
        }
    }

    #[test]
    fn a_zoomed_in_graph_can_still_be_dragged_across() {
        // The clamp must not be so tight that zooming in pins the graph:
        // at 4x most of it is off-pane by design, and the reader has to be
        // able to reach the far side.
        let scene = scene_of(&["a.rs", "b.rs", "c.rs", "d.rs"], &[("a.rs", "b.rs")]);
        let zoomed = View::default().zoomed_at(4.0, (600.0, 350.0), PANE);
        let left = zoomed.panned((-400.0, 0.0)).clamped(&scene, PANE);
        let right = zoomed.panned((400.0, 0.0)).clamped(&scene, PANE);
        assert_ne!(left.pan.0, right.pan.0, "the graph is pinned in place");
    }

    #[test]
    fn panning_moves_the_picture_by_exactly_the_drag() {
        let view = View::default().panned((30.0, -12.0));
        assert_eq!(view.to_screen((0.0, 0.0), PANE), (30.0, -12.0));
    }

    #[test]
    fn a_click_finds_the_node_under_it_and_nothing_when_there_is_none() {
        let scene = scene_of(&["a.rs", "b.rs", "c.rs"], &[("a.rs", "b.rs"), ("b.rs", "c.rs")]);
        let view = View::default();
        for (at, node) in scene.nodes.iter().enumerate() {
            let centre = view.to_screen(node.centre, PANE);
            assert_eq!(hit_test(&scene, PANE, view, centre), Some(at), "{}", node.name);
        }
        assert_eq!(hit_test(&scene, PANE, view, (0.5, 0.5)), None, "the corner is empty");
    }

    #[test]
    fn a_press_that_stayed_put_is_a_click_and_one_that_travelled_is_a_drag() {
        // The regression this exists for: dragging the canvas used to end by
        // opening whatever file the drag started on, because the movement
        // was measured against the last mouse-move rather than the press.
        assert!(is_click((100.0, 100.0), (100.0, 100.0)), "a still press is a click");
        assert!(is_click((100.0, 100.0), (102.0, 97.0)), "a shaky hand is still a click");
        assert!(!is_click((100.0, 100.0), (360.0, 20.0)), "a long drag is not a click");
        assert!(!is_click((100.0, 100.0), (100.0, 140.0)), "vertical counts too");
        assert!(!is_click((100.0, 100.0), (140.0, 100.0)), "horizontal counts too");
    }

    #[test]
    fn a_click_outside_the_pane_hits_nothing() {
        let scene = scene_of(&["a.rs", "b.rs"], &[("a.rs", "b.rs")]);
        assert_eq!(hit_test(&scene, PANE, View::default(), (-40.0, -40.0)), None);
    }

    #[test]
    fn hit_testing_follows_the_graph_as_it_is_panned_and_zoomed() {
        // The bug this guards: hit-testing against un-transformed positions
        // opens the wrong file the moment the reader moves the graph.
        let scene = scene_of(&["a.rs", "b.rs", "c.rs"], &[("a.rs", "b.rs")]);
        let view = View::default().panned((120.0, -40.0)).zoomed_at(1.7, (400.0, 300.0), PANE);
        for (at, node) in scene.nodes.iter().enumerate() {
            let centre = view.to_screen(node.centre, PANE);
            if !PANE.contains(centre.0, centre.1) {
                continue;
            }
            assert_eq!(hit_test(&scene, PANE, view, centre), Some(at), "{}", node.name);
        }
    }

    // --- drawing --------------------------------------------------------

    #[test]
    fn the_traced_node_and_its_edges_are_picked_out() {
        let scene = scene_of(&["a.rs", "b.rs", "c.rs"], &[("a.rs", "b.rs"), ("b.rs", "c.rs")]);
        let theme = Theme::dark();
        let plain = pane_quads(&scene, PANE, View::default(), &theme, None);
        let traced = pane_quads(&scene, PANE, View::default(), &theme, Some(0));

        assert!(traced.len() > plain.len(), "the ring adds a quad");
        assert!(
            traced.iter().any(|quad| quad.color.srgb == theme.text.srgb),
            "something is drawn in the highlight colour"
        );
        assert!(
            !plain.iter().any(|quad| quad.color.srgb == theme.text.srgb),
            "and nothing is, when nothing is traced"
        );
    }

    #[test]
    fn an_edge_leaving_the_pane_is_shortened_rather_than_dropped() {
        assert_eq!(
            clip_segment((-50.0, 50.0), (150.0, 50.0), Rect::new(0.0, 0.0, 100.0, 100.0)),
            Some(((0.0, 50.0), (100.0, 50.0)))
        );
    }

    #[test]
    fn an_edge_that_misses_the_pane_entirely_is_dropped() {
        assert_eq!(
            clip_segment((200.0, 200.0), (300.0, 300.0), Rect::new(0.0, 0.0, 100.0, 100.0)),
            None
        );
    }

    #[test]
    fn a_node_half_off_the_pane_is_dropped_rather_than_painted_over_the_chrome() {
        let pane = Rect::new(100.0, 50.0, 200.0, 100.0);
        assert!(inside(pane, (110.0, 60.0), 40.0, Theme::dark().error).is_none());
        assert!(inside(pane, (200.0, 100.0), 20.0, Theme::dark().error).is_some());
    }

    // --- labels ---------------------------------------------------------

    #[test]
    fn a_label_is_centred_on_its_node() {
        let scene = scene_of(&["main.rs", "parser.rs"], &[("main.rs", "parser.rs")]);
        let rows = label_rows(&scene, PANE, View::default(), METRICS, None).text;
        assert!(rows.contains("main"), "{rows}");
    }

    #[test]
    fn zooming_in_fills_names_out() {
        // A small circle can only carry a few characters; zooming in should
        // buy the reader the rest of the name, not just bigger dots.
        let mut names = vec!["workspace_search.rs"];
        let others: Vec<String> = (0..12).map(|index| format!("other{index}.rs")).collect();
        names.extend(others.iter().map(String::as_str));
        let edges: Vec<(&str, &str)> =
            others.iter().map(|other| ("workspace_search.rs", other.as_str())).collect();
        let scene = scene_of(&names, &edges);

        // Zoomed the way the wheel does it -- anchored on the node -- so
        // the hub stays under the pointer instead of flying off the pane.
        let hub = scene
            .nodes
            .iter()
            .position(|node| node.name == "workspace_search.rs")
            .expect("the hub is drawn");
        let far = View::default();
        let on_hub = far.to_screen(scene.nodes[hub].centre, PANE);
        let close = far.zoomed_at(6.0, on_hub, PANE);
        let rows = label_rows(&scene, PANE, close, METRICS, None).text;
        assert!(rows.contains("workspace_search.rs"), "zoomed in: {rows}");
    }

    #[test]
    fn the_traced_node_keeps_its_label_whatever_else_wanted_the_room() {
        let scene = scene_of(&["a.rs", "b.rs", "c.rs"], &[("a.rs", "b.rs")]);
        let traced = 2;
        let rows = label_rows(&scene, PANE, View::default(), METRICS, Some(traced)).text;
        assert!(rows.contains(&scene.nodes[traced].name), "{rows}");
    }

    #[test]
    fn labels_that_would_touch_are_kept_a_space_apart() {
        // Two names run together read as a third filename that does not
        // exist, so the second is dropped rather than abutted.
        let scene = Scene {
            nodes: vec![
                Node {
                    centre: (40.0, 20.0),
                    radius: 30.0,
                    path: PathBuf::from("aaa"),
                    name: "aaa".into(),
                    imported: false,
                },
                Node {
                    centre: (64.0, 20.0),
                    radius: 30.0,
                    path: PathBuf::from("bbb"),
                    name: "bbb".into(),
                    imported: false,
                },
            ],
            width: 1200.0,
            height: 700.0,
            ..Scene::default()
        };
        let rows = label_rows(&scene, PANE, View::default(), METRICS, None).text;
        assert!(!rows.contains("aaabbb"), "ran together: {rows}");
        assert!(rows.contains("aaa"));
    }

    #[test]
    fn a_label_off_the_pane_is_not_stamped() {
        let scene = Scene {
            nodes: vec![Node {
                centre: (40.0, 20.0),
                radius: 30.0,
                path: PathBuf::from("far.rs"),
                name: "far.rs".into(),
                imported: false,
            }],
            width: 1200.0,
            height: 700.0,
            ..Scene::default()
        };
        let away = View::default().panned((0.0, -5000.0));
        assert_eq!(label_rows(&scene, PANE, away, METRICS, None).text.trim(), "");
    }

    /// Where a name is actually drawn on screen, or `None` if it was not
    /// stamped at all.
    fn drawn_at(grid: &LabelGrid, name: &str, metrics: GridMetrics) -> Option<(f32, f32)> {
        for (row, line) in grid.text.split('\n').enumerate() {
            if let Some(column) = line.find(name) {
                // `find` gives a byte offset; the grid is filled with spaces
                // and ASCII filenames here, so it is also the column.
                return Some((
                    grid.origin.0 + column as f32 * metrics.digit_width,
                    grid.origin.1 + row as f32 * metrics.line_height,
                ));
            }
        }
        None
    }

    #[test]
    fn a_label_travels_with_its_circle_instead_of_shivering() {
        // The bug this exists for. Labels are quantised to a character cell,
        // and quantising against the pane meant each name jumped its cell at
        // a different sub-pixel moment during a drag -- so the whole picture
        // shivered and none of it could be read while moving. Pinning the
        // grid to the graph makes every label move by exactly the drag.
        let scene = scene_of(
            &["main.rs", "parser.rs", "token.rs", "lexer.rs"],
            &[("main.rs", "parser.rs"), ("parser.rs", "token.rs"), ("main.rs", "lexer.rs")],
        );
        let name = "main.rs";
        let start = label_rows(&scene, PANE, View::default(), METRICS, None);
        let from = drawn_at(&start, name, METRICS).expect("main.rs is labelled");

        // Step across a whole cell in both axes, a pixel at a time: this is
        // exactly where the old code flipped cells and jumped.
        for step in 1..=24 {
            let by = (step as f32 * 0.5, step as f32 * 0.9);
            let view = View::default().panned(by);
            let grid = label_rows(&scene, PANE, view, METRICS, None);
            let now = drawn_at(&grid, name, METRICS).expect("main.rs is still labelled");
            let moved = (now.0 - from.0, now.1 - from.1);
            assert!(
                (moved.0 - by.0).abs() < 0.01 && (moved.1 - by.1).abs() < 0.01,
                "panned by {by:?} but the label moved {moved:?}"
            );
        }
    }

    #[test]
    fn a_label_sits_within_half_a_cell_of_its_own_circle() {
        // The quantisation that is left is fixed, but it still has to be
        // small enough that a name reads as belonging to its circle.
        let scene = scene_of(&["main.rs", "parser.rs"], &[("main.rs", "parser.rs")]);
        let view = View::default();
        let grid = label_rows(&scene, PANE, view, METRICS, None);
        let node = scene.nodes.iter().find(|node| node.name == "main.rs").expect("main.rs");
        let centre = view.to_screen(node.centre, PANE);
        let drawn = drawn_at(&grid, "main.rs", METRICS).expect("main.rs is labelled");

        let middle = drawn.0 + "main.rs".len() as f32 * METRICS.digit_width / 2.0;
        assert!(
            (middle - centre.0).abs() <= METRICS.digit_width / 2.0 + 0.01,
            "label centre {middle} vs circle centre {}",
            centre.0
        );
        assert!(
            (drawn.1 - centre.1).abs() <= METRICS.line_height / 2.0 + 0.01,
            "label row {} vs circle centre {}",
            drawn.1,
            centre.1
        );
    }

    #[test]
    fn a_name_is_cut_with_an_ellipsis_that_counts_towards_the_room() {
        assert_eq!(cut("workspace_search.rs", 8), "workspa\u{2026}");
        assert_eq!(cut("app.rs", 8), "app.rs");
    }

    // --- the cache ------------------------------------------------------

    #[test]
    fn a_settled_graph_survives_the_round_trip() {
        let root = Path::new("/work/app");
        let settled = settle_graph(&graph_of(
            &["main.rs", "src/parser.rs", "a file with spaces.rs"],
            &[("main.rs", "src/parser.rs")],
        ));
        let decoded = decode(root, &encode(root, &settled)).expect("its own output reads back");
        assert_eq!(decoded.files, settled.files, "paths with spaces survive");
        assert_eq!(decoded.edges, settled.edges);
        assert_eq!(decoded.truncated, settled.truncated);
        for (at, point) in decoded.positions.iter().enumerate() {
            assert!((point.0 - settled.positions[at].0).abs() < 0.01);
            assert!((point.1 - settled.positions[at].1).abs() < 0.01);
        }
    }

    #[test]
    fn a_cache_written_for_another_folder_is_refused() {
        // Otherwise a moved or renamed workspace would open showing the
        // wrong repository's graph.
        let settled = settle_graph(&graph_of(&["a.rs"], &[]));
        let text = encode(Path::new("/work/app"), &settled);
        assert!(decode(Path::new("/play/app"), &text).is_none());
        assert!(decode(Path::new("/work/app"), &text).is_some());
    }

    #[test]
    fn a_cache_from_another_version_is_refused() {
        let settled = settle_graph(&graph_of(&["a.rs"], &[]));
        let text = encode(Path::new("/work/app"), &settled)
            .replace(CACHE_VERSION, "lightspeed-depgraph 0");
        assert!(decode(Path::new("/work/app"), &text).is_none());
    }

    #[test]
    fn a_damaged_cache_is_refused_rather_than_half_read() {
        let root = Path::new("/work/app");
        let settled =
            settle_graph(&graph_of(&["a.rs", "b.rs"], &[("a.rs", "b.rs")]));
        let whole = encode(root, &settled);

        // Cut off part-way through.
        let cut = whole.lines().take(4).collect::<Vec<_>>().join("\n");
        assert!(decode(root, &cut).is_none(), "a truncated file is not half-read");

        // An edge pointing at a file that is not there.
        let bent = whole.replace("\n0 1\n", "\n0 99\n");
        assert!(decode(root, &bent).is_none(), "an out-of-range edge is refused");

        // Junk where a number should be.
        let junk = whole.replace("files 2", "files banana");
        assert!(decode(root, &junk).is_none());
    }

    #[test]
    fn an_empty_graph_caches_and_reads_back_as_empty() {
        let root = Path::new("/work/empty");
        let settled = settle_graph(&graph_of(&[], &[]));
        let decoded = decode(root, &encode(root, &settled)).expect("an empty graph is cacheable");
        assert!(decoded.files.is_empty());
    }
}

#[cfg(test)]
mod real_repo {
    use super::*;

    /// Diagnostic, not a gate: reports what this repository lays out to, so
    /// "does the graph fit on a screen" is a measured answer.
    /// `cargo test -p lightspeed --bin lightspeed -- --ignored --nocapture real_repo`
    #[test]
    #[ignore = "measures this checkout; not a correctness gate"]
    fn extent_of_this_repository() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
        let started = std::time::Instant::now();
        let graph = ls_core::dependency_graph::build(&root);
        let scanned = started.elapsed();

        let started = std::time::Instant::now();
        let settled = settle_graph(&graph);
        let simulated = started.elapsed();

        let viewport = (1200.0, 700.0);
        let started = std::time::Instant::now();
        let scene = build_scene(&settled, viewport);
        let fitted = started.elapsed();

        let started = std::time::Instant::now();
        let text = encode(&root, &settled);
        let encoded = started.elapsed();
        let started = std::time::Instant::now();
        let read_back = decode(&root, &text).expect("its own output reads back");
        let decoded = started.elapsed();
        assert_eq!(read_back.files.len(), settled.files.len());

        let pane = Rect::new(60.0, 100.0, viewport.0, viewport.1);
        let quads = pane_quads(&scene, pane, View::default(), &Theme::dark(), None);
        let shown = quads.iter().filter(|q| q.shape == crate::quads::Shape::Ellipse).count();
        let radii: Vec<f32> = scene.nodes.iter().map(|node| node.radius).collect();
        println!(
            "{} files, {} edges ({} drawn), scan {scanned:?}, settle {simulated:?}, fit {fitted:?}",
            graph.files.len(),
            graph.edges.len(),
            scene.segments.len() / 3,
        );
        println!(
            "  {shown} of {} nodes on screen, radii {:.1}..{:.1} px, {} quads",
            scene.nodes.len(),
            radii.iter().copied().fold(f32::MAX, f32::min),
            radii.iter().copied().fold(0.0_f32, f32::max),
            quads.len()
        );
        println!(
            "  cache {} bytes, encode {encoded:?}, decode {decoded:?}",
            text.len()
        );
    }
}
