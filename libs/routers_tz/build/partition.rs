use geo::{Contains, Coord, GeoNum, Kernel, Orientation, Point};
use rayon::prelude::*;
use routers_tz_types::timezone::internal::TimezoneBuild;
use std::collections::HashMap;

const PARALLEL_EDGES: usize = 4096;

pub trait Region: Clone + PartialEq + Send + Sync {
    fn bounds(&self) -> Bounds;
    /// Must not lie on a polygon edge.
    fn centre(&self) -> Coord;
    fn is_finest(&self) -> bool;
    fn children(&self) -> [Self; 4];
}

/// Planar (lng, lat) bounds; `lng` holds two ranges where they cross the antimeridian.
pub struct Bounds {
    pub lat: (f64, f64),
    pub lng: [(f64, f64); 2],
}

impl Bounds {
    fn touches(&self, edge: &Edge) -> bool {
        let overlaps = |(lo, hi): (f64, f64), p: f64, q: f64| p.min(q) <= hi && p.max(q) >= lo;
        overlaps(self.lat, edge.a.y, edge.b.y)
            && (self.lng.iter()).any(|&lng| overlaps(lng, edge.a.x, edge.b.x))
    }
}

/// Each region comes with the sorted zones that hold it; overlapping polygons give several.
pub fn partition<R: Region>(roots: Vec<R>, timezones: &[TimezoneBuild]) -> Vec<(R, Vec<u16>)> {
    let edges = collect_edges(timezones);
    roots
        .into_par_iter()
        .flat_map(|root| descend(&Node::root(root, &edges, timezones), timezones))
        .collect()
}

#[derive(Clone, Copy)]
struct Edge {
    zone: u16,
    a: Coord,
    b: Coord,
}

/// Ordered by zone, so each zone's edges are contiguous.
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

struct Node<R> {
    region: R,
    centre: Coord,
    edges: Vec<Edge>,
    /// Zones that hold the whole region.
    inside: Vec<u16>,
    /// Zones with an edge in the region that hold its centre.
    centre_in: Vec<u16>,
}

impl<R: Region> Node<R> {
    fn root(region: R, edges: &[Edge], timezones: &[TimezoneBuild]) -> Self {
        let bounds = region.bounds();
        let centre = region.centre();
        let edges: Vec<Edge> = edges
            .iter()
            .filter(|e| bounds.touches(e))
            .copied()
            .collect();
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
        let edges: Vec<Edge> = (self.edges.iter())
            .filter(|e| bounds.touches(e))
            .copied()
            .collect();
        let boundary = zones_of(&edges);

        // A segment that wraps the antimeridian leaves the parent's bounds, so its crossings are unknown.
        let wraps = (centre.x - self.centre.x).abs() > 180.0;
        let contained: Vec<u16> = self
            .edges
            .chunk_by(|a, b| a.zone == b.zone)
            .filter_map(|run| {
                let zone = run[0].zone;
                let is_in = if wraps {
                    contains(timezones, zone, centre)
                } else {
                    let crossings = (run.iter())
                        .filter(|e| crosses(self.centre, centre, e))
                        .count();
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

    fn owners(&self) -> Vec<u16> {
        let mut owners = [&self.inside[..], &self.centre_in[..]].concat();
        owners.sort_unstable();
        if owners.is_empty() {
            let nearest = (self.edges.iter())
                .min_by(|a, b| distance(self.centre, a).total_cmp(&distance(self.centre, b)))
                .expect("an owned region touches at least one zone");
            owners.push(nearest.zone);
        }
        owners
    }
}

/// Drops regions that touch no zone and collapses four siblings with the same zones into their parent.
fn descend<R: Region>(node: &Node<R>, timezones: &[TimezoneBuild]) -> Vec<(R, Vec<u16>)> {
    let mut present = zones_of(&node.edges);
    present.extend(&node.inside);
    present.sort_unstable();
    present.dedup();

    match present[..] {
        [] => return vec![],
        [zone] => return vec![(node.region.clone(), vec![zone])],
        _ if node.edges.is_empty() => return vec![(node.region.clone(), node.inside.clone())],
        _ if node.region.is_finest() => return vec![(node.region.clone(), node.owners())],
        _ => {}
    }

    let children = node.region.children();
    let visit = |child: &R| descend(&node.child(child.clone(), timezones), timezones);
    let regions: Vec<(R, Vec<u16>)> = if node.edges.len() >= PARALLEL_EDGES {
        children.par_iter().flat_map(visit).collect()
    } else {
        children.iter().flat_map(visit).collect()
    };

    let collapses = regions.len() == 4
        && (regions.iter().zip(&children)).all(|((region, _), child)| region == child)
        && (regions.iter()).all(|(_, zones)| *zones == regions[0].1);
    if collapses {
        vec![(node.region.clone(), regions[0].1.clone())]
    } else {
        regions
    }
}

/// Stored zone indices: a lone zone keeps its own, and each overlap set takes the next index past the last zone.
pub struct ZoneSets {
    zones: usize,
    pub overlaps: Vec<Vec<u16>>,
    seen: HashMap<Vec<u16>, u16>,
}

impl ZoneSets {
    pub fn new(zones: usize) -> Self {
        ZoneSets {
            zones,
            overlaps: Vec::new(),
            seen: HashMap::new(),
        }
    }

    pub fn index(&mut self, zones: Vec<u16>) -> u16 {
        if let [zone] = zones[..] {
            return zone;
        }
        *self.seen.entry(zones).or_insert_with_key(|zones| {
            self.overlaps.push(zones.clone());
            (self.zones + self.overlaps.len() - 1) as u16
        })
    }
}

fn zones_of(edges: &[Edge]) -> Vec<u16> {
    (edges.chunk_by(|a, b| a.zone == b.zone))
        .map(|run| run[0].zone)
        .collect()
}

fn contains(timezones: &[TimezoneBuild], zone: u16, at: Coord) -> bool {
    timezones[zone as usize].geometry.0.contains(&Point(at))
}

/// Collinear counts as clockwise, so a crossing through a shared vertex counts once.
fn crosses(p: Coord, q: Coord, edge: &Edge) -> bool {
    let ccw = |a, b, c| <f64 as GeoNum>::Ker::orient2d(a, b, c) == Orientation::CounterClockwise;
    ccw(p, q, edge.a) != ccw(p, q, edge.b) && ccw(edge.a, edge.b, p) != ccw(edge.a, edge.b, q)
}

fn distance(at: Coord, edge: &Edge) -> f64 {
    use geo::{Distance, Euclidean, Line};
    Euclidean.distance(&Point(at), &Line::new(edge.a, edge.b))
}
