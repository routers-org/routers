//! Splits the globe into regions that each belong to exactly one timezone.
//!
//! Descent starts from a set of root regions. Each region keeps the polygon
//! edges that touch its bounds, and divides until only one zone touches it.
//! Containment of a child's centre follows from its parent's by counting edge
//! crossings along the segment between the two centres.

use geo::{Contains, Coord, GeoNum, Kernel, Orientation, Point};
use rayon::prelude::*;
use routers_tz_types::timezone::internal::TimezoneBuild;

/// Regions with fewer edges than this are partitioned on the current thread.
const PARALLEL_EDGES: usize = 4096;

/// A region of the globe that divides into four children.
pub trait Region: Clone + PartialEq + Send + Sync {
    fn bounds(&self) -> Bounds;
    /// A point inside the region that should not lie on a polygon edge.
    fn centre(&self) -> Coord;
    fn is_finest(&self) -> bool;
    fn children(&self) -> [Self; 4];
}

/// A planar (lng, lat) box, split in two where it crosses the antimeridian.
pub struct Bounds {
    pub lat: (f64, f64),
    pub lng: [(f64, f64); 2],
}

impl Bounds {
    fn touches(&self, edge: &Edge) -> bool {
        let overlaps = |(lo, hi): (f64, f64), p: f64, q: f64| p.min(q) <= hi && p.max(q) >= lo;
        overlaps(self.lat, edge.a.y, edge.b.y)
            && self.lng.iter().any(|&lng| overlaps(lng, edge.a.x, edge.b.x))
    }
}

/// Partitions every root into regions with one zone each, as (region, zone index).
pub fn partition<R: Region>(roots: Vec<R>, timezones: &[TimezoneBuild]) -> Vec<(R, u16)> {
    let edges = collect_edges(timezones);
    eprintln!(
        "[partition] {} edges across {} timezones from {} roots",
        edges.len(),
        timezones.len(),
        roots.len()
    );

    roots
        .into_par_iter()
        .flat_map(|root| descend(&Node::root(root, &edges, timezones), timezones))
        .collect()
}

/// One ring segment of a timezone polygon, in planar (lng, lat) degrees.
#[derive(Clone, Copy)]
struct Edge {
    zone: u16,
    a: Coord,
    b: Coord,
}

/// Every ring edge, ordered by zone so that each zone's edges are contiguous.
fn collect_edges(timezones: &[TimezoneBuild]) -> Vec<Edge> {
    timezones
        .iter()
        .enumerate()
        .flat_map(|(zone, tz)| {
            tz.geometry.0.iter().flat_map(move |polygon| {
                core::iter::once(polygon.exterior())
                    .chain(polygon.interiors())
                    .flat_map(move |ring| {
                        ring.lines().map(move |line| Edge {
                            zone: zone as u16,
                            a: line.start,
                            b: line.end,
                        })
                    })
            })
        })
        .collect()
}

/// A region during partitioning, with the zone state the descent carries down.
struct Node<R> {
    region: R,
    centre: Coord,
    /// Edges whose bounds touch the region, ordered by zone.
    edges: Vec<Edge>,
    /// Zones that contain the whole region (no edge touches it).
    inside: Vec<u16>,
    /// Zones with an edge in the region that contain its centre.
    centre_in: Vec<u16>,
}

impl<R: Region> Node<R> {
    /// A root region, with containment of its centre tested against every zone.
    fn root(region: R, edges: &[Edge], timezones: &[TimezoneBuild]) -> Self {
        let bounds = region.bounds();
        let centre = region.centre();
        let edges: Vec<Edge> = edges.iter().filter(|e| bounds.touches(e)).copied().collect();
        let boundary = zones_of(&edges);

        let (centre_in, inside) = (0..timezones.len() as u16)
            .filter(|&zone| contains(timezones, zone, centre))
            .partition(|zone| boundary.binary_search(zone).is_ok());

        Node {
            region,
            centre,
            edges,
            inside,
            centre_in,
        }
    }

    fn child(&self, region: R, timezones: &[TimezoneBuild]) -> Self {
        let bounds = region.bounds();
        let centre = region.centre();
        let edges: Vec<Edge> = self.edges.iter().filter(|e| bounds.touches(e)).copied().collect();
        let boundary = zones_of(&edges);

        // A segment that wraps the antimeridian leaves the parent's box, so its crossings are unknown.
        let wraps = (centre.x - self.centre.x).abs() > 180.0;
        let contained: Vec<u16> = self
            .edges
            .chunk_by(|a, b| a.zone == b.zone)
            .filter_map(|run| {
                let zone = run[0].zone;
                let is_in = if wraps {
                    contains(timezones, zone, centre)
                } else {
                    let crossings = run.iter().filter(|e| crosses(self.centre, centre, e)).count();
                    self.centre_in.binary_search(&zone).is_ok() ^ (crossings % 2 == 1)
                };
                is_in.then_some(zone)
            })
            .collect();

        let (centre_in, newly_inside): (Vec<u16>, Vec<u16>) = contained
            .into_iter()
            .partition(|zone| boundary.binary_search(zone).is_ok());

        let mut inside = self.inside.clone();
        inside.extend(newly_inside);
        inside.sort_unstable();

        Node {
            region,
            centre,
            edges,
            inside,
            centre_in,
        }
    }

    /// The zone a finest region is assigned to when several zones touch it.
    fn owner(&self) -> u16 {
        let nearest_edge = || {
            self.edges
                .iter()
                .min_by(|a, b| distance(self.centre, a).total_cmp(&distance(self.centre, b)))
                .map(|e| e.zone)
        };

        (self.inside.first().copied())
            .or_else(|| self.centre_in.first().copied())
            .or_else(nearest_edge)
            .expect("an owned region touches at least one zone")
    }
}

/// Regions that touch no zone are dropped; four siblings with one zone collapse into their parent.
fn descend<R: Region>(node: &Node<R>, timezones: &[TimezoneBuild]) -> Vec<(R, u16)> {
    let mut present = zones_of(&node.edges);
    present.extend(&node.inside);
    present.sort_unstable();
    present.dedup();

    match present[..] {
        [] => return vec![],
        [zone] => return vec![(node.region.clone(), zone)],
        _ if node.region.is_finest() => return vec![(node.region.clone(), node.owner())],
        _ => {}
    }

    let children = node.region.children();
    let visit = |child: &R| descend(&node.child(child.clone(), timezones), timezones);
    let regions: Vec<(R, u16)> = if node.edges.len() >= PARALLEL_EDGES {
        children.par_iter().flat_map(visit).collect()
    } else {
        children.iter().flat_map(visit).collect()
    };

    let collapses = regions.len() == 4
        && regions.iter().zip(&children).all(|((region, _), child)| region == child)
        && regions.iter().all(|(_, zone)| *zone == regions[0].1);
    if collapses {
        vec![(node.region.clone(), regions[0].1)]
    } else {
        regions
    }
}

/// The distinct zones of edges that are ordered by zone.
fn zones_of(edges: &[Edge]) -> Vec<u16> {
    edges.chunk_by(|a, b| a.zone == b.zone).map(|run| run[0].zone).collect()
}

fn contains(timezones: &[TimezoneBuild], zone: u16, at: Coord) -> bool {
    timezones[zone as usize].geometry.0.contains(&Point(at))
}

/// Whether segment `p`–`q` crosses `edge`. Collinear points count as clockwise,
/// so a crossing exactly through a shared vertex is counted once.
fn crosses(p: Coord, q: Coord, edge: &Edge) -> bool {
    let ccw = |a, b, c| <f64 as GeoNum>::Ker::orient2d(a, b, c) == Orientation::CounterClockwise;
    ccw(p, q, edge.a) != ccw(p, q, edge.b) && ccw(edge.a, edge.b, p) != ccw(edge.a, edge.b, q)
}

fn distance(at: Coord, edge: &Edge) -> f64 {
    use geo::{Distance, Euclidean, Line};
    Euclidean.distance(&Point(at), &Line::new(edge.a, edge.b))
}
