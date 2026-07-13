//! Shared boundary arcs for categorical terrain fields.
//!
//! Every grid edge separating two labels is stored once, stitched into a
//! maximal arc, simplified once, then referenced in opposite directions by the
//! adjacent faces. This keeps shade bands gap-free after simplification.

use std::collections::{BTreeMap, BTreeSet};

pub(super) type Point = (i32, i32);
type LabelPair = (i8, i8);

const PENALTY_TOLERANCE: f64 = 1.0;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct EdgeKey {
    start: Point,
    end: Point,
}

impl EdgeKey {
    fn new(a: Point, b: Point) -> Self {
        if a <= b {
            Self { start: a, end: b }
        } else {
            Self { start: b, end: a }
        }
    }

    fn other(self, point: Point) -> Point {
        if point == self.start {
            self.end
        } else {
            debug_assert_eq!(point, self.end);
            self.start
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct EdgeInfo {
    labels: LabelPair,
    /// Label on the right while traversing `EdgeKey::start -> EdgeKey::end`.
    right: i8,
    left: i8,
}

#[derive(Debug)]
struct SharedArc {
    points: Vec<Point>,
    right: i8,
    left: i8,
    start_direction: i32,
    end_direction: i32,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DirectedArc {
    id: usize,
    reversed: bool,
}

/// Traces and simplifies all non-neutral faces from a buffered label grid.
/// `origin` maps local cell `(0, 0)` into the unbuffered grid coordinate space;
/// `tile_size` marks the fixed tile-border anchors at 0 and `tile_size`.
pub(super) fn trace_shared_rings(
    labels: &[i8],
    width: usize,
    height: usize,
    origin: i32,
    tile_size: i32,
) -> BTreeMap<i8, Vec<Vec<Point>>> {
    let (edges, adjacency) = boundary_graph(labels, width, height, origin);
    let arcs = build_arcs(&edges, &adjacency, tile_size);
    assemble_rings(&arcs)
}

fn boundary_graph(
    labels: &[i8],
    width: usize,
    height: usize,
    origin: i32,
) -> (BTreeMap<EdgeKey, EdgeInfo>, BTreeMap<Point, Vec<EdgeKey>>) {
    let label_at = |x: isize, y: isize| -> i8 {
        if x < 0 || y < 0 || x >= width as isize || y >= height as isize {
            0
        } else {
            labels[y as usize * width + x as usize]
        }
    };
    let mut edges = BTreeMap::new();

    // Canonical vertical direction is top -> bottom; its right side is west.
    for y in 0..height as i32 {
        for x in 0..=width as i32 {
            let west = label_at(x as isize - 1, y as isize);
            let east = label_at(x as isize, y as isize);
            if west != east {
                insert_edge(
                    &mut edges,
                    (origin + x, origin + y),
                    (origin + x, origin + y + 1),
                    west,
                    east,
                );
            }
        }
    }

    // Canonical horizontal direction is left -> right; its right side is south.
    for y in 0..=height as i32 {
        for x in 0..width as i32 {
            let north = label_at(x as isize, y as isize - 1);
            let south = label_at(x as isize, y as isize);
            if north != south {
                insert_edge(
                    &mut edges,
                    (origin + x, origin + y),
                    (origin + x + 1, origin + y),
                    south,
                    north,
                );
            }
        }
    }

    let mut adjacency: BTreeMap<Point, Vec<EdgeKey>> = BTreeMap::new();
    for edge in edges.keys().copied() {
        adjacency.entry(edge.start).or_default().push(edge);
        adjacency.entry(edge.end).or_default().push(edge);
    }
    (edges, adjacency)
}

fn insert_edge(
    edges: &mut BTreeMap<EdgeKey, EdgeInfo>,
    start: Point,
    end: Point,
    right: i8,
    left: i8,
) {
    let key = EdgeKey::new(start, end);
    debug_assert_eq!(key.start, start);
    let labels = if right < left {
        (right, left)
    } else {
        (left, right)
    };
    let previous = edges.insert(
        key,
        EdgeInfo {
            labels,
            right,
            left,
        },
    );
    debug_assert!(previous.is_none());
}

fn build_arcs(
    edges: &BTreeMap<EdgeKey, EdgeInfo>,
    adjacency: &BTreeMap<Point, Vec<EdgeKey>>,
    tile_size: i32,
) -> Vec<SharedArc> {
    let mut remaining = edges.keys().copied().collect::<BTreeSet<_>>();
    let mut arcs = Vec::new();
    while let Some(seed) = remaining.first().copied() {
        let pair = edges[&seed].labels;
        remaining.remove(&seed);
        let mut points = vec![seed.start, seed.end];
        let closed = extend_arc(
            &mut points,
            false,
            pair,
            &mut remaining,
            adjacency,
            edges,
            tile_size,
        );
        if !closed {
            extend_arc(
                &mut points,
                true,
                pair,
                &mut remaining,
                adjacency,
                edges,
                tile_size,
            );
        }

        let first_edge = EdgeKey::new(points[0], points[1]);
        let first_info = edges[&first_edge];
        let forward = first_edge.start == points[0];
        let (right, left) = if forward {
            (first_info.right, first_info.left)
        } else {
            (first_info.left, first_info.right)
        };
        debug_assert!(points.windows(2).all(|segment| {
            let edge = EdgeKey::new(segment[0], segment[1]);
            let info = edges[&edge];
            let forward = edge.start == segment[0];
            let edge_right = if forward { info.right } else { info.left };
            edge_right == right
        }));

        let raw_start_direction = direction(points[0], points[1]);
        let raw_end_direction = direction(points[points.len() - 2], points[points.len() - 1]);
        let points = simplify_arc(&points, right, left);
        arcs.push(SharedArc {
            points,
            right,
            left,
            start_direction: raw_start_direction,
            end_direction: raw_end_direction,
        });
    }
    arcs
}

/// Extends one end of an arc until a junction, tile boundary, or loop closure.
/// Walking both ends is necessary because the ordered seed edge may lie in the
/// middle of an otherwise open chain.
fn extend_arc(
    points: &mut Vec<Point>,
    prepend: bool,
    pair: LabelPair,
    remaining: &mut BTreeSet<EdgeKey>,
    adjacency: &BTreeMap<Point, Vec<EdgeKey>>,
    edges: &BTreeMap<EdgeKey, EdgeInfo>,
    tile_size: i32,
) -> bool {
    let mut prefix = Vec::new();
    loop {
        let current = if prepend {
            prefix.last().copied().unwrap_or(points[0])
        } else {
            *points.last().expect("arc has endpoints")
        };
        if is_arc_endpoint(current, pair, adjacency, edges, tile_size) {
            finish_prefix(points, prefix);
            return false;
        }
        let candidates = adjacency[&current]
            .iter()
            .copied()
            .filter(|edge| remaining.contains(edge) && edges[edge].labels == pair)
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            finish_prefix(points, prefix);
            return false;
        }
        let edge = candidates[0];
        remaining.remove(&edge);
        let next = edge.other(current);
        if prepend {
            prefix.push(next);
        } else {
            points.push(next);
        }
        if !prepend && points.first() == points.last() {
            return true;
        }
    }
}

fn finish_prefix(points: &mut Vec<Point>, mut prefix: Vec<Point>) {
    prefix.reverse();
    prefix.append(points);
    *points = prefix;
}

fn is_arc_endpoint(
    point: Point,
    pair: LabelPair,
    adjacency: &BTreeMap<Point, Vec<EdgeKey>>,
    edges: &BTreeMap<EdgeKey, EdgeInfo>,
    tile_size: i32,
) -> bool {
    if is_tile_anchor(point, pair, adjacency, edges, tile_size) {
        return true;
    }
    adjacency
        .get(&point)
        .into_iter()
        .flatten()
        .filter(|edge| edges[*edge].labels == pair)
        .count()
        != 2
}

fn is_tile_anchor(
    point: Point,
    pair: LabelPair,
    adjacency: &BTreeMap<Point, Vec<EdgeKey>>,
    edges: &BTreeMap<EdgeKey, EdgeInfo>,
    tile_size: i32,
) -> bool {
    let on_x = point.0 == 0 || point.0 == tile_size;
    let on_y = point.1 == 0 || point.1 == tile_size;
    if on_x && on_y {
        return true;
    }
    let mut incident = adjacency
        .get(&point)
        .into_iter()
        .flatten()
        .filter(|edge| edges[*edge].labels == pair);
    if on_x {
        return incident
            .clone()
            .any(|edge| edge.start.0 != point.0 || edge.end.0 != point.0);
    }
    if on_y {
        return incident.any(|edge| edge.start.1 != point.1 || edge.end.1 != point.1);
    }
    false
}

fn assemble_rings(arcs: &[SharedArc]) -> BTreeMap<i8, Vec<Vec<Point>>> {
    let mut outgoing: BTreeMap<i8, BTreeMap<Point, Vec<DirectedArc>>> = BTreeMap::new();
    for (id, arc) in arcs.iter().enumerate() {
        for (label, reversed) in [(arc.right, false), (arc.left, true)] {
            if label == 0 {
                continue;
            }
            let directed = DirectedArc { id, reversed };
            outgoing
                .entry(label)
                .or_default()
                .entry(directed_start(arc, directed))
                .or_default()
                .push(directed);
        }
    }
    for by_point in outgoing.values_mut() {
        for candidates in by_point.values_mut() {
            candidates.sort_unstable();
        }
    }

    let mut result = BTreeMap::new();
    for (label, mut by_point) in outgoing {
        let mut rings = Vec::new();
        while let Some((start, first)) = take_first_arc(&mut by_point) {
            let first_arc = &arcs[first.id];
            let mut ring = directed_points(first_arc, first);
            let mut current = directed_end(first_arc, first);
            let mut incoming = directed_end_direction(first_arc, first);
            let mut closed = current == start;

            for _ in 0..=arcs.len() {
                if closed {
                    break;
                }
                let Some(next) = take_turning_arc(&mut by_point, current, incoming, arcs) else {
                    break;
                };
                let arc = &arcs[next.id];
                let points = directed_points(arc, next);
                ring.extend(points.into_iter().skip(1));
                current = directed_end(arc, next);
                incoming = directed_end_direction(arc, next);
                closed = current == start;
            }
            if closed && ring.len() >= 4 {
                if ring.first() != ring.last() {
                    ring.push(ring[0]);
                }
                rings.push(ring);
            }
        }
        if !rings.is_empty() {
            result.insert(label, rings);
        }
    }
    result
}

fn take_first_arc(
    outgoing: &mut BTreeMap<Point, Vec<DirectedArc>>,
) -> Option<(Point, DirectedArc)> {
    let point = outgoing
        .iter()
        .find_map(|(point, arcs)| (!arcs.is_empty()).then_some(*point))?;
    let arc = outgoing.get_mut(&point)?.remove(0);
    Some((point, arc))
}

fn take_turning_arc(
    outgoing: &mut BTreeMap<Point, Vec<DirectedArc>>,
    point: Point,
    incoming: i32,
    arcs: &[SharedArc],
) -> Option<DirectedArc> {
    let candidates = outgoing.get_mut(&point)?;
    let (index, _) = candidates.iter().enumerate().min_by_key(|(_, directed)| {
        let outgoing = directed_start_direction(&arcs[directed.id], **directed);
        let turn = (outgoing - incoming).rem_euclid(4);
        let rank = match turn {
            1 => 0,
            0 => 1,
            3 => 2,
            _ => 3,
        };
        (rank, **directed)
    })?;
    Some(candidates.remove(index))
}

fn directed_points(arc: &SharedArc, directed: DirectedArc) -> Vec<Point> {
    if directed.reversed {
        arc.points.iter().rev().copied().collect()
    } else {
        arc.points.clone()
    }
}

fn directed_start(arc: &SharedArc, directed: DirectedArc) -> Point {
    if directed.reversed {
        *arc.points.last().expect("arc has endpoints")
    } else {
        arc.points[0]
    }
}

fn directed_end(arc: &SharedArc, directed: DirectedArc) -> Point {
    if directed.reversed {
        arc.points[0]
    } else {
        *arc.points.last().expect("arc has endpoints")
    }
}

fn directed_start_direction(arc: &SharedArc, directed: DirectedArc) -> i32 {
    if directed.reversed {
        (arc.end_direction + 2).rem_euclid(4)
    } else {
        arc.start_direction
    }
}

fn directed_end_direction(arc: &SharedArc, directed: DirectedArc) -> i32 {
    if directed.reversed {
        (arc.start_direction + 2).rem_euclid(4)
    } else {
        arc.end_direction
    }
}

/// VTracer-inspired polygon mode: combine straight runs, remove one-pixel
/// staircases by signed area, then greedily replace low-penalty subpaths.
/// Adapted from visioncortex's MIT/Apache-2.0 `PathSimplify` implementation.
fn simplify_arc(points: &[Point], right: i8, left: i8) -> Vec<Point> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let closed = points.first() == points.last();
    let mut simplified = remove_collinear(points, closed);
    let clockwise = if closed {
        signed_area(&simplified) > 0
    } else {
        right > left
    };
    simplified = remove_staircases(&simplified, closed, clockwise);
    simplified = limit_penalties(&simplified, PENALTY_TOLERANCE);
    let minimum = if closed { 4 } else { 2 };
    if simplified.len() < minimum {
        points.to_vec()
    } else {
        simplified
    }
}

fn remove_collinear(points: &[Point], closed: bool) -> Vec<Point> {
    let mut points = points.to_vec();
    if closed && points.first() == points.last() {
        points.pop();
    }
    loop {
        if points.len() <= if closed { 3 } else { 2 } {
            break;
        }
        let mut keep = vec![true; points.len()];
        let range = if closed {
            0..points.len()
        } else {
            1..points.len() - 1
        };
        for index in range {
            let previous = if index == 0 {
                points.len() - 1
            } else {
                index - 1
            };
            let next = (index + 1) % points.len();
            if triangle_area(points[previous], points[index], points[next]) == 0 {
                keep[index] = false;
            }
        }
        if keep.iter().all(|keep| *keep) {
            break;
        }
        let mut index = 0;
        points.retain(|_| {
            let keep = keep[index];
            index += 1;
            keep
        });
    }
    if closed && !points.is_empty() {
        points.push(points[0]);
    }
    points
}

fn remove_staircases(points: &[Point], closed: bool, clockwise: bool) -> Vec<Point> {
    let mut path = points;
    if closed && points.first() == points.last() {
        path = &points[..points.len() - 1];
    }
    if path.len() <= 2 {
        return points.to_vec();
    }
    let mut result = Vec::with_capacity(points.len());
    for index in 0..path.len() {
        let previous = if index == 0 {
            path.len() - 1
        } else {
            index - 1
        };
        let next = (index + 1) % path.len();
        let endpoint = !closed && (index == 0 || index + 1 == path.len());
        let unit_stair =
            manhattan(path[index], path[previous]) == 1 || manhattan(path[index], path[next]) == 1;
        let area = triangle_area(path[previous], path[index], path[next]);
        if endpoint || !unit_stair || (area != 0 && (area > 0) == clockwise) {
            result.push(path[index]);
        }
    }
    if closed && !result.is_empty() {
        result.push(result[0]);
    }
    result
}

fn limit_penalties(points: &[Point], tolerance: f64) -> Vec<Point> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let mut result = vec![points[0]];
    let mut last = 0;
    for index in 1..points.len() {
        if index == last + 1 {
            if index + 1 == points.len() {
                result.push(points[index]);
            }
            continue;
        }
        let penalty = (last + 1..index)
            .map(|between| evaluate_penalty(points[last], points[between], points[index]))
            .fold(0.0_f64, f64::max);
        if penalty >= tolerance {
            last = index - 1;
            result.push(points[last]);
        }
        if index + 1 == points.len() {
            result.push(points[index]);
        }
    }
    result.dedup();
    result
}

fn evaluate_penalty(a: Point, b: Point, c: Point) -> f64 {
    let base = squared_distance(a, c).sqrt();
    if base == 0.0 {
        return f64::INFINITY;
    }
    let area2 = triangle_area(a, b, c).unsigned_abs() as f64;
    // Heron's `area² / base` from VTracer, expressed through the cross product.
    area2 * area2 / (4.0 * base)
}

fn squared_distance(a: Point, b: Point) -> f64 {
    let dx = f64::from(a.0 - b.0);
    let dy = f64::from(a.1 - b.1);
    dx * dx + dy * dy
}

fn manhattan(a: Point, b: Point) -> i32 {
    (a.0 - b.0).abs() + (a.1 - b.1).abs()
}

fn triangle_area(a: Point, b: Point, c: Point) -> i64 {
    i64::from(b.0 - a.0) * i64::from(c.1 - a.1) - i64::from(c.0 - a.0) * i64::from(b.1 - a.1)
}

fn signed_area(points: &[Point]) -> i64 {
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
        .map(|(a, b)| i64::from(a.0) * i64::from(b.1) - i64::from(a.1) * i64::from(b.0))
        .sum()
}

fn direction(from: Point, to: Point) -> i32 {
    match ((to.0 - from.0).signum(), (to.1 - from.1).signum()) {
        (1, 0) => 0,
        (0, 1) => 1,
        (-1, 0) => 2,
        (0, -1) => 3,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staircase_is_reduced_to_a_shared_diagonal() {
        let points = vec![(0, 0), (1, 0), (1, 1), (2, 1), (2, 2), (3, 2)];
        let simplified = simplify_arc(&points, 2, 1);
        assert!(simplified.len() < points.len());
        assert_eq!(simplified.first(), points.first());
        assert_eq!(simplified.last(), points.last());
    }

    #[test]
    fn shared_boundary_is_stored_once() {
        let labels = [1, 2, 1, 2];
        let (edges, adjacency) = boundary_graph(&labels, 2, 2, 0);
        let arcs = build_arcs(&edges, &adjacency, 2);
        let shared = arcs
            .iter()
            .filter(|arc| (arc.left, arc.right) == (1, 2) || (arc.left, arc.right) == (2, 1))
            .count();
        assert_eq!(shared, 1);
    }

    #[test]
    fn tile_boundary_points_split_arcs() {
        let labels = [1, 1, 2, 2];
        let (edges, adjacency) = boundary_graph(&labels, 2, 2, -1);
        let arcs = build_arcs(&edges, &adjacency, 1);
        assert!(arcs.iter().all(|arc| {
            let interior = &arc.points[1..arc.points.len().saturating_sub(1)];
            interior
                .iter()
                .all(|point| point.0 != 0 && point.1 != 0 && point.0 != 1 && point.1 != 1)
        }));
    }
}
