//! End-to-end GeoParquet reader test.
//!
//! Generates a tiny connector/segment parquet pair at runtime (no committed
//! binary fixture), then reads it back through [`read_transportation`] and
//! [`OvertureNetwork::from_geoparquet`], exercising the full parquet →
//! serde_arrow → wkb → graph pipeline.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{BinaryArray, StringArray};
use arrow::datatypes::{DataType, Field, FieldRef, Schema};
use arrow::record_batch::RecordBatch;
use geo::point;
use parquet::arrow::ArrowWriter;
use routers_network::{Discovery, Route, Scan};
use serde::Serialize;
use serde_arrow::schema::{SchemaLike, TracingOptions};

use super::read_transportation;
use crate::overture::graph::OvertureNetwork;
use crate::overture::parsers::{RoadClass, TravelMode};

// --- Serializable mirrors of the segment schema (write side) --------------

#[derive(Serialize)]
struct SegOut {
    id: String,
    subtype: Option<String>,
    class: Option<String>,
    subclass: Option<String>,
    connectors: Vec<ConnOut>,
    speed_limits: Vec<SpeedLimitOut>,
    access_restrictions: Vec<AccessOut>,
}

#[derive(Serialize)]
struct ConnOut {
    connector_id: String,
    at: f64,
}

#[derive(Serialize)]
struct SpeedOut {
    value: i64,
    unit: String,
}

#[derive(Serialize)]
struct VehicleOut {
    dimension: String,
    comparison: String,
    value: f64,
    unit: String,
}

#[derive(Serialize, Default)]
struct WhenOut {
    heading: Option<String>,
    mode: Option<Vec<String>>,
    during: Option<String>,
    using: Option<Vec<String>>,
    recognized: Option<Vec<String>>,
    vehicle: Option<Vec<VehicleOut>>,
}

#[derive(Serialize)]
struct SpeedLimitOut {
    min_speed: Option<SpeedOut>,
    max_speed: Option<SpeedOut>,
    when: Option<WhenOut>,
}

#[derive(Serialize)]
struct AccessOut {
    access_type: Option<String>,
    when: Option<WhenOut>,
}

/// Little-endian WKB encoding of a 2D point.
fn wkb_point(x: f64, y: f64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(21);
    buf.push(1u8); // little-endian byte order
    buf.extend_from_slice(&1u32.to_le_bytes()); // geometry type 1 = Point
    buf.extend_from_slice(&x.to_le_bytes());
    buf.extend_from_slice(&y.to_le_bytes());
    buf
}

fn write_parquet(path: &Path, batch: &RecordBatch) {
    let file = File::create(path).expect("create parquet");
    let mut writer = ArrowWriter::try_new(file, batch.schema(), None).expect("writer");
    writer.write(batch).expect("write batch");
    writer.close().expect("close writer");
}

fn write_connectors(path: &Path) {
    let ids = StringArray::from(vec!["c1", "c2", "c3"]);
    let geoms = BinaryArray::from(
        [
            wkb_point(0.0, 0.0),
            wkb_point(1.0, 0.0),
            wkb_point(2.0, 0.0),
        ]
        .iter()
        .map(|v| v.as_slice())
        .collect::<Vec<_>>(),
    );

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("geometry", DataType::Binary, false),
    ]));
    let batch = RecordBatch::try_new(schema, vec![Arc::new(ids), Arc::new(geoms)])
        .expect("connector batch");
    write_parquet(path, &batch);
}

fn segment_samples() -> Vec<SegOut> {
    let when = || WhenOut {
        heading: Some("forward".into()),
        mode: Some(vec!["car".into()]),
        ..Default::default()
    };
    vec![
        SegOut {
            id: "s100".into(),
            subtype: Some("road".into()),
            class: Some("primary".into()),
            subclass: None,
            connectors: vec![
                ConnOut {
                    connector_id: "c1".into(),
                    at: 0.0,
                },
                ConnOut {
                    connector_id: "c2".into(),
                    at: 0.5,
                },
                ConnOut {
                    connector_id: "c3".into(),
                    at: 1.0,
                },
            ],
            speed_limits: vec![SpeedLimitOut {
                min_speed: Some(SpeedOut {
                    value: 30,
                    unit: "km/h".into(),
                }),
                max_speed: Some(SpeedOut {
                    value: 50,
                    unit: "km/h".into(),
                }),
                when: Some(when()),
            }],
            access_restrictions: vec![],
        },
        SegOut {
            id: "s200".into(),
            subtype: Some("road".into()),
            class: Some("primary".into()),
            subclass: Some("link".into()),
            connectors: vec![
                ConnOut {
                    connector_id: "c2".into(),
                    at: 0.0,
                },
                ConnOut {
                    connector_id: "c3".into(),
                    at: 1.0,
                },
            ],
            speed_limits: vec![],
            access_restrictions: vec![AccessOut {
                access_type: Some("denied".into()),
                when: Some(when()),
            }],
        },
        // A non-road segment that must be ignored by the reader.
        SegOut {
            id: "w1".into(),
            subtype: Some("water".into()),
            class: None,
            subclass: None,
            connectors: vec![
                ConnOut {
                    connector_id: "c1".into(),
                    at: 0.0,
                },
                ConnOut {
                    connector_id: "c3".into(),
                    at: 1.0,
                },
            ],
            speed_limits: vec![],
            access_restrictions: vec![],
        },
    ]
}

/// Segments exercising the conditional (`when`) and malformed-value paths.
fn edge_case_samples() -> Vec<SegOut> {
    vec![
        // Over-height denial: conditional, must not close the road.
        SegOut {
            id: "s300".into(),
            subtype: Some("road".into()),
            class: Some("residential".into()),
            subclass: None,
            connectors: vec![
                ConnOut {
                    connector_id: "c1".into(),
                    at: 0.0,
                },
                ConnOut {
                    connector_id: "c2".into(),
                    at: 1.0,
                },
            ],
            speed_limits: vec![],
            access_restrictions: vec![AccessOut {
                access_type: Some("denied".into()),
                when: Some(WhenOut {
                    vehicle: Some(vec![VehicleOut {
                        dimension: "height".into(),
                        comparison: "greater_than".into(),
                        value: 4.2,
                        unit: "m".into(),
                    }]),
                    ..Default::default()
                }),
            }],
        },
        // Peak-hour directional denial: conditional despite its heading.
        SegOut {
            id: "s400".into(),
            subtype: Some("road".into()),
            class: Some("residential".into()),
            subclass: None,
            connectors: vec![
                ConnOut {
                    connector_id: "c2".into(),
                    at: 0.0,
                },
                ConnOut {
                    connector_id: "c3".into(),
                    at: 1.0,
                },
            ],
            speed_limits: vec![],
            access_restrictions: vec![AccessOut {
                access_type: Some("denied".into()),
                when: Some(WhenOut {
                    heading: Some("forward".into()),
                    during: Some("Mo-Fr 07:00-09:00".into()),
                    ..Default::default()
                }),
            }],
        },
        // School-zone limit (conditional) beside a blanket limit; the mode
        // list carries an unknown value that must be filtered, and the
        // min_speed carries an unknown unit that must be dropped.
        SegOut {
            id: "s500".into(),
            subtype: Some("road".into()),
            class: Some("tertiary".into()),
            subclass: None,
            connectors: vec![
                ConnOut {
                    connector_id: "c1".into(),
                    at: 0.0,
                },
                ConnOut {
                    connector_id: "c3".into(),
                    at: 1.0,
                },
            ],
            speed_limits: vec![
                SpeedLimitOut {
                    min_speed: None,
                    max_speed: Some(SpeedOut {
                        value: 40,
                        unit: "km/h".into(),
                    }),
                    when: Some(WhenOut {
                        during: Some("Mo-Fr 08:00-09:30,14:30-16:00".into()),
                        ..Default::default()
                    }),
                },
                SpeedLimitOut {
                    min_speed: Some(SpeedOut {
                        value: 20,
                        unit: "furlongs".into(),
                    }),
                    max_speed: Some(SpeedOut {
                        value: 60,
                        unit: "km/h".into(),
                    }),
                    when: Some(WhenOut {
                        mode: Some(vec!["car".into(), "spaceship".into()]),
                        ..Default::default()
                    }),
                },
            ],
            access_restrictions: vec![
                // Purpose/status-scoped rules: conditional.
                AccessOut {
                    access_type: Some("allowed".into()),
                    when: Some(WhenOut {
                        using: Some(vec!["at_destination".into()]),
                        recognized: Some(vec!["as_private".into()]),
                        ..Default::default()
                    }),
                },
                // Unknown access type: dropped entirely.
                AccessOut {
                    access_type: Some("teleported".into()),
                    when: None,
                },
            ],
        },
        // Unknown road class: retained by the reader, dropped by the builder.
        SegOut {
            id: "s600".into(),
            subtype: Some("road".into()),
            class: Some("hyperloop".into()),
            subclass: None,
            connectors: vec![
                ConnOut {
                    connector_id: "c1".into(),
                    at: 0.0,
                },
                ConnOut {
                    connector_id: "c2".into(),
                    at: 1.0,
                },
            ],
            speed_limits: vec![],
            access_restrictions: vec![],
        },
    ]
}

fn write_segments(path: &Path, samples: &[SegOut]) {
    let options = TracingOptions::default().allow_null_fields(true);
    let fields = Vec::<FieldRef>::from_samples(&samples, options).expect("trace schema");
    let batch = serde_arrow::to_record_batch(&fields, &samples).expect("segment batch");
    write_parquet(path, &batch);
}

/// A fresh temp directory laid out like an Overture partition. `name` keeps
/// concurrent tests from sharing (and clobbering) the same path.
fn fixture_dir(name: &str, samples: &[SegOut]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("routers_overture_reader_test_{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("type=connector")).expect("mkdir connector");
    fs::create_dir_all(dir.join("type=segment")).expect("mkdir segment");
    write_connectors(&dir.join("type=connector/part-0.parquet"));
    write_segments(&dir.join("type=segment/part-0.parquet"), samples);
    dir
}

#[test]
fn reads_transportation_geoparquet_into_elements() {
    let dir = fixture_dir("elements", &segment_samples());
    let transportation = read_transportation(&dir).expect("read");

    assert_eq!(
        transportation.connectors.len(),
        3,
        "three connectors decoded"
    );
    // Two road segments kept; the water segment dropped.
    assert_eq!(
        transportation.segments.len(),
        2,
        "only road segments retained"
    );

    // Connector geometry decoded from WKB.
    let xs: Vec<f64> = transportation
        .connectors
        .iter()
        .map(|c| c.position.x())
        .collect();
    assert!(xs.contains(&0.0) && xs.contains(&2.0));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn builds_network_from_geoparquet() {
    let dir = fixture_dir("network", &segment_samples());
    let net = OvertureNetwork::from_geoparquet(&dir).expect("build network");

    assert_eq!(net.num_nodes(), 3);
    // c1↔c2 and c2↔c3 → four directed edges.
    assert_eq!(net.num_edges(), 4);

    // The chain is routable end to end.
    let route = net.route_points(&point! { x: 0.0, y: 0.0 }, &point! { x: 2.0, y: 0.0 });
    assert!(route.is_some(), "route across the segment chain");

    // The 50 km/h max speed survived parquet → serde_arrow → domain.
    let has_speed = net.segments().map(|(_, m)| m).any(|m| {
        m.speed_limits
            .iter()
            .any(|s| s.max_speed.map(|v| v.in_kmh()) == Some(50))
    });
    assert!(has_speed, "parsed max speed limit present");

    // A nearest-node lookup confirms the spatial index is populated.
    assert!(net.nearest_node(&point! { x: 0.1, y: 0.0 }).is_some());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn conditional_restrictions_survive_the_round_trip() {
    let dir = fixture_dir("conditional", &edge_case_samples());
    let transportation = read_transportation(&dir).expect("read");
    let by_class = |class: RoadClass| {
        transportation
            .segments
            .iter()
            .find(|s| s.road_class == Some(class))
            .expect("segment present")
    };

    // s300: over-height denial — conditional, unheaded, mode-less.
    let height_limited = by_class(RoadClass::Residential);
    let denial = &height_limited.access[0];
    assert!(denial.conditional, "vehicle-dimension rule is conditional");
    assert!(denial.heading.is_none());
    assert!(denial.mode.is_empty());

    // s500: conditional school-zone limit beside a blanket limit; unknown
    // mode values filtered, unknown speed unit dropped.
    let school_zone = by_class(RoadClass::Tertiary);
    let [zoned, blanket] = &school_zone.speed_limits[..] else {
        panic!("two speed limits expected");
    };
    assert!(zoned.conditional, "hour-of-day limit is conditional");
    assert_eq!(zoned.max_speed.map(|s| s.in_kmh()), Some(40));
    assert!(!blanket.conditional);
    assert_eq!(blanket.max_speed.map(|s| s.in_kmh()), Some(60));
    assert_eq!(blanket.mode, vec![TravelMode::Car], "unknown mode filtered");
    assert!(blanket.min_speed.is_none(), "unknown unit dropped");

    // s500: purpose/status-scoped allow is conditional; the rule with an
    // unrecognised access type is dropped entirely.
    assert_eq!(school_zone.access.len(), 1);
    assert!(school_zone.access[0].conditional);

    // s600: unknown class is retained by the reader without a road class.
    assert!(
        transportation
            .segments
            .iter()
            .any(|s| s.road_class.is_none()),
        "unknown-class segment retained"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn conditional_denials_do_not_close_roads() {
    let dir = fixture_dir("open", &edge_case_samples());
    let net = OvertureNetwork::from_geoparquet(&dir).expect("build network");

    // Every navigable street stays open in both directions: the over-height
    // (s300) and peak-hour (s400) denials only hold conditionally.
    for (source, target, _) in net.edges() {
        assert!(
            net.edge(&target, &source).is_some(),
            "conditional denial must not close a direction"
        );
    }

    // The unknown-class segment contributes no edges or metadata.
    let classes: Vec<_> = net
        .segments()
        .map(|(_, m)| m)
        .filter_map(|m| m.road_class)
        .collect();
    assert_eq!(
        net.segments().count(),
        classes.len(),
        "every graph segment classed"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn when_scope_conditionality() {
    use super::rows::{VehicleConditionRow, WhenRow};

    let bare = WhenRow {
        heading: Some("forward".into()),
        mode: Some(vec!["car".into()]),
        during: None,
        using: None,
        recognized: None,
        vehicle: None,
    };
    assert!(!bare.is_conditional(), "heading/mode alone are evaluated");

    // Empty condition arrays impose nothing.
    let empty = WhenRow {
        vehicle: Some(Vec::new()),
        using: Some(Vec::new()),
        recognized: Some(Vec::new()),
        ..bare
    };
    assert!(!empty.is_conditional());

    let timed = WhenRow {
        during: Some("Su 09:00-18:00".into()),
        heading: None,
        mode: None,
        using: None,
        recognized: None,
        vehicle: None,
    };
    assert!(timed.is_conditional());

    let dimensioned = WhenRow {
        vehicle: Some(vec![VehicleConditionRow {
            dimension: Some("weight".into()),
            comparison: Some("greater_than".into()),
            value: Some(3.5),
            unit: Some("t".into()),
        }]),
        during: None,
        heading: None,
        mode: None,
        using: None,
        recognized: None,
    };
    assert!(dimensioned.is_conditional());
}
