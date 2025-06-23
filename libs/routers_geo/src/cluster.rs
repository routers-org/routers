use geo::{Centroid, ConvexHull, Coord, Distance, Haversine, LineString, Point, Polygon};
use log::error;
use std::collections::HashMap;
use strum::{EnumCount, EnumIter, EnumProperty, VariantArray};
#[cfg(feature = "tile")]
use wkt::ToWkt;

use crate::coord::point::FeatureKey;
use crate::error::GeoError;
#[cfg(feature = "tile")]
use crate::{geo::TileItem, routers_codec::mvt::Value};

#[derive(PartialEq, Clone)]
pub enum Classification {
    Core(usize),
    Edge(usize),
    Noise,
}

/// Describes a set of clustered points, with a centroid of the cluster and
/// the respective convex hull describing the cluster's shape.
#[derive(Clone)]
pub struct Clustered<T> {
    pub id: u64,
    pub points: Vec<T>,
    pub convex_hull: Polygon,

    centroid: Point,
}

impl<T> From<Clustered<T>> for Point {
    fn from(val: Clustered<T>) -> Self {
        val.centroid
    }
}

/// The cluster contains noise (spillage, i.e. elements that have no relevant cluster)
/// and the clustered elements.
pub struct Cluster<T> {
    // a.k.a Spillage
    pub noise: Vec<T>,
    pub clustered: Vec<Clustered<T>>,
}

#[derive(EnumCount, EnumProperty, EnumIter, VariantArray, strum::Display, Copy, Clone)]
pub enum ClusteredFeatureKeys {
    NumberOfPoints,
    ConvexHull,
}

impl FeatureKey for ClusteredFeatureKeys {}

#[cfg(feature = "tile")]
impl<T: Clone> TileItem<Value> for Clustered<T> {
    type Key = ClusteredFeatureKeys;

    fn id(&self) -> u64 {
        self.id
    }

    fn entries(&self) -> Vec<(Self::Key, Value)> {
        vec![
            (
                Self::Key::NumberOfPoints,
                Value::from_int(self.points.len() as i64),
            ),
            (
                Self::Key::ConvexHull,
                Value::from_string(self.convex_hull.wkt_string()),
            ),
        ]
    }
}

// Packs a level-8 geohash into a u64
pub fn geohash_to_u64(geohash: &str) -> Option<u64> {
    let base32_alphabet = "0123456789bcdefghjkmnpqrstuvwxyz";
    let mut result: u64 = 0;

    if geohash.len() > 8 {
        return None; // Geohash length must be <= 8 characters to fit in 64 bits
    }

    for (i, c) in geohash.chars().enumerate() {
        if let Some(index) = base32_alphabet.find(c) {
            // Map 5-bit index to 8-bit value by shifting and padding
            let byte_value = (index as u8) << 3; // Shift left by 3 to fit into 8-bit space
            result |= (byte_value as u64) << (8 * (7 - i)); // Shift and accumulate in u64
        } else {
            return None; // Invalid character in the geohash
        }
    }

    Some(result)
}

impl<T: Into<geo::Point> + Clone> TryFrom<Vec<T>> for Clustered<T> {
    type Error = GeoError;

    fn try_from(points: Vec<T>) -> Result<Self, Self::Error> {
        let value = points
            .iter()
            .cloned()
            .map(|point| Into::<geo::Point>::into(point))
            .collect::<Vec<geo::Point>>();

        let polygon = Polygon::new(LineString::from(value), vec![]);

        let convex_hull = polygon.convex_hull();
        let centroid = convex_hull
            .centroid()
            .ok_or(GeoError::InvalidCoordinate("".to_string()))?;

        let encoding = geohash::encode(Coord::from(centroid), 8).map_err(GeoError::GeoHashError)?;

        let id = geohash_to_u64(&encoding).ok_or(GeoError::InvalidCoordinate(
            "GeoHash too wide, expected depth of 8.".parse().unwrap(),
        ))?;

        Ok(Self {
            id,
            points,
            centroid,
            convex_hull,
        })
    }
}

impl<T> From<Vec<T>> for Cluster<T> {
    fn from(value: Vec<T>) -> Self {
        Self {
            noise: value,
            clustered: Vec::new(),
        }
    }
}

impl<T: Into<geo::Point> + Clone> TryFrom<Vec<(u32, T)>> for Cluster<T> {
    type Error = GeoError;

    fn try_from(value: Vec<(u32, T)>) -> Result<Self, GeoError> {
        let mut hashmap: HashMap<u32, Vec<T>> = HashMap::new();
        value
            .into_iter()
            .for_each(|(hash, p)| match hashmap.get_mut(&hash) {
                None => {
                    hashmap.insert(hash, vec![p]);
                }
                Some(group) => {
                    group.push(p);
                }
            });

        let grouped: Vec<Vec<T>> = hashmap.into_values().collect();

        let mut clustered: Vec<Clustered<T>> = vec![];
        let mut noise: Vec<T> = vec![];

        grouped.into_iter().for_each(|group| {
            if group.len() >= 3 {
                match Clustered::try_from(group) {
                    Ok(cluster) => clustered.push(cluster),
                    Err(error) => {
                        error!("Failed to cluster, {:?}", error)
                    }
                }
            } else {
                noise.extend(group);
            }
        });

        Ok(Self { clustered, noise })
    }
}

/// A method to cluster elements, into a [`Cluster`].
/// Using IntoCluster::cluster.
///
/// Example:
/// ```rust
/// use geo::{Point, point, coord};
/// use routers_geo::cluster::IntoCluster;
///
/// fn cluster() {
///     let points: Vec<Point> = vec![
///         point!(coord! { x: 1.0, y: 0.0 }),
///         point!(coord! { x: 2f64, y: 1f64 }),
///         point!(coord! { x: 100f64, y: 0f64 })
///     ];
///
///     let clustered = IntoCluster::new()
///         .cluster(points)
///         .expect("Must cluster");
///
///     println!("{} Clusters and {} spilled nodes.", clustered.clustered.len(), clustered.noise.len());
/// }
/// ```
///
pub struct IntoCluster {
    pub epsilon: f64,
    pub c_capacity: usize,

    distance: fn(geo::Point, geo::Point) -> f64,
    c: Vec<Classification>,
    v: Vec<bool>,
}

impl Default for IntoCluster {
    fn default() -> Self {
        IntoCluster {
            epsilon: 1.0,
            c_capacity: 10,
            distance: |a, b| Haversine.distance(a, b),
            c: Vec::new(),
            v: Vec::new(),
        }
    }
}

impl IntoCluster {
    pub fn new() -> Self {
        IntoCluster::default()
    }

    pub fn distance(self, distance: fn(_: geo::Point, _: geo::Point) -> f64) -> Self {
        Self { distance, ..self }
    }

    #[inline]
    fn range_query(&self, sample: geo::Point, population: &[geo::Point]) -> Vec<usize> {
        population
            .iter()
            .enumerate()
            .filter(|(_, pt)| (self.distance)(sample, **pt) < self.epsilon)
            .map(|(idx, _)| idx)
            .collect()
    }

    #[inline]
    fn expand_cluster(
        &mut self,
        population: &[geo::Point],
        queue: &mut Vec<usize>,
        cluster: usize,
    ) -> bool {
        let mut new_cluster = false;
        while let Some(ind) = queue.pop() {
            let neighbors = self.range_query(population[ind], population);
            if neighbors.len() < self.c_capacity {
                continue;
            }

            new_cluster = true;
            self.c[ind] = Classification::Core(cluster);

            for n_idx in neighbors {
                // n_idx is at least an edge point
                if self.c[n_idx] == Classification::Noise {
                    self.c[n_idx] = Classification::Edge(cluster);
                }

                if self.v[n_idx] {
                    continue;
                }

                self.v[n_idx] = true;
                queue.push(n_idx);
            }
        }
        new_cluster
    }

    pub fn cluster<T: Into<geo::Point> + Clone>(
        mut self,
        population: Vec<T>,
    ) -> Result<Cluster<T>, GeoError> {
        self.c = vec![Classification::Noise; population.len()];
        self.v = vec![false; population.len()];

        let as_points = population
            .iter()
            .cloned()
            .map(|p| p.into())
            .collect::<Vec<geo::Point>>();

        let mut cluster = 0;
        let mut queue = Vec::new();

        for idx in 0..population.len() {
            if self.v[idx] {
                continue;
            }

            self.v[idx] = true;

            queue.push(idx);

            if self.expand_cluster(as_points.as_slice(), &mut queue, cluster) {
                cluster += 1;
            }
        }

        let points: Vec<(u32, _)> = self
            .c
            .iter()
            .zip(population)
            .map(|(p, c)| match p {
                Classification::Core(id) => (*id as u32 + 1, c),
                _ => (0, c),
            })
            .collect();

        Cluster::try_from(points)
    }
}
