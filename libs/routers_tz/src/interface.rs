use core::fmt::Debug;
use geo::{BoundingRect, Polygon, Rect};
use routers_tz_types::TimeZone;

pub trait TimezoneResolver {
    type Error: Debug + From<()>;

    fn search(&self, rect: &Rect) -> Result<Vec<TimeZone>, Self::Error>;

    fn search_polygon(&self, polygon: &Polygon) -> Result<Vec<TimeZone>, Self::Error> {
        polygon
            .bounding_rect()
            .ok_or(Self::Error::from(()))
            .and_then(|rect| self.search(&rect))
    }
}

#[cfg(test)]
#[cfg(any(feature = "rtree", feature = "basic", feature = "s2cell"))]
mod tests {
    use crate::TimezoneResolver;

    use geo::{BoundingRect, point};
    use std::sync::OnceLock;

    type Resolver = dyn TimezoneResolver<Error = ()> + Sync;

    #[cfg(feature = "rtree")]
    static RTREE: OnceLock<crate::RTreeStorage> = OnceLock::new();

    #[cfg(feature = "basic")]
    static BASIC: OnceLock<crate::BasicStorage> = OnceLock::new();

    #[cfg(feature = "s2cell")]
    static S2CELL: OnceLock<crate::S2CellStorage> = OnceLock::new();

    #[ctor::ctor]
    fn init() {
        #[cfg(feature = "rtree")]
        RTREE.get_or_init(crate::RTreeStorage::default);

        #[cfg(feature = "basic")]
        BASIC.get_or_init(crate::BasicStorage::default);

        #[cfg(feature = "s2cell")]
        S2CELL.get_or_init(crate::S2CellStorage::default);
    }

    pub fn resolvers() -> Vec<(&'static str, &'static Resolver)> {
        [
            #[cfg(feature = "rtree")]
            (
                "rtree",
                RTREE.get().expect("initialised") as &'static Resolver,
            ),
            #[cfg(feature = "basic")]
            (
                "basic",
                BASIC.get().expect("initialised") as &'static Resolver,
            ),
            #[cfg(feature = "s2cell")]
            (
                "s2cell",
                S2CELL.get().expect("initialised") as &'static Resolver,
            ),
        ]
        .to_vec()
    }

    fn assert_zones(cases: &[(f64, f64, &[&str])]) {
        let failures: Vec<String> = resolvers()
            .into_iter()
            .flat_map(|(backend, resolver)| {
                cases.iter().filter_map(move |&(x, y, expected)| {
                    let found = resolver.search(&point! { x: x, y: y }.bounding_rect());
                    let mut names = found.map(|zones| {
                        zones
                            .iter()
                            .map(|z| z.name().to_string())
                            .collect::<Vec<_>>()
                    });
                    names.iter_mut().for_each(|names| names.sort_unstable());
                    (!names.as_ref().is_ok_and(|names| names.iter().eq(expected))).then(|| {
                        format!("{backend}: ({x}, {y}) expected {expected:?}, found {names:?}")
                    })
                })
            })
            .collect();

        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    fn assert_located(cases: &[(f64, f64, &str)]) {
        let cases: Vec<(f64, f64, &[&str])> = (cases.iter())
            .map(|(x, y, zone)| (*x, *y, core::slice::from_ref(zone)))
            .collect();
        assert_zones(&cases);
    }

    #[test]
    fn locate_cities() {
        assert_located(&[
            (151.208211, -33.871075, "Australia/Sydney"),
            (-87.64, 41.86350, "America/Chicago"),
            (8.540560425944761, 47.373334621336284, "Europe/Zurich"),
            (2.3522, 48.8566, "Europe/Paris"),
            (-0.1276, 51.5072, "Europe/London"),
            (139.6917, 35.6895, "Asia/Tokyo"),
            (-74.0060, 40.7128, "America/New_York"),
            (-118.2437, 34.0522, "America/Los_Angeles"),
            (-46.6333, -23.5505, "America/Sao_Paulo"),
            (28.0473, -26.2041, "Africa/Johannesburg"),
            (37.6173, 55.7558, "Europe/Moscow"),
        ]);
    }

    // Split from America/Ojinaga in tzdata 2022g.
    #[test]
    fn locate_ciudad_juarez() {
        assert_located(&[(-106.45, 31.45, "America/Ciudad_Juarez")]);
    }

    #[test]
    fn locate_across_the_antimeridian() {
        assert_located(&[
            (174.7633, -36.8485, "Pacific/Auckland"),
            (174.7762, -41.2865, "Pacific/Auckland"),
            (172.6362, -43.5321, "Pacific/Auckland"),
            (-176.55, -43.95, "Pacific/Chatham"),
            (178.4419, -18.1416, "Pacific/Fiji"),
            (177.5, 64.73, "Asia/Anadyr"),
            (-176.65, 51.88, "America/Adak"),
            (-171.7667, -13.8333, "Pacific/Apia"),
            (-175.2049, -21.1394, "Pacific/Tongatapu"),
            (-157.4278, 1.8721, "Pacific/Kiritimati"),
        ]);
    }

    #[test]
    fn locate_either_side_of_a_border() {
        assert_located(&[
            (-106.3778, 31.8072, "America/Denver"),
            (-106.4245, 31.6904, "America/Ciudad_Juarez"),
            (-83.07, 42.36, "America/Detroit"),
            (-83.0, 42.28, "America/Toronto"),
            (146.92, -36.05, "Australia/Sydney"),
            (146.8878, -36.1218, "Australia/Melbourne"),
            (6.1432, 46.2044, "Europe/Zurich"),
            (7.8134, 48.5737, "Europe/Berlin"),
            (-86.1581, 39.7684, "America/Indiana/Indianapolis"),
            (-87.3464, 41.5934, "America/Chicago"),
        ]);
    }

    #[test]
    fn locate_enclaves() {
        assert_located(&[
            (-111.6513, 35.1983, "America/Phoenix"),
            (-110.2546, 36.7275, "America/Denver"),
            (-110.50, 35.80, "America/Phoenix"),
            (141.4538, -31.9539, "Australia/Broken_Hill"),
        ]);
    }

    #[test]
    fn locate_unusual_offsets() {
        assert_located(&[
            (159.0821, -31.5553, "Australia/Lord_Howe"),
            (128.8893, -31.6773, "Australia/Eucla"),
            (88.3639, 22.5726, "Asia/Kolkata"),
            (85.3240, 27.7172, "Asia/Kathmandu"),
            (69.2075, 34.5553, "Asia/Kabul"),
            (51.3890, 35.6892, "Asia/Tehran"),
        ]);
    }

    #[test]
    fn locate_overlapping_zones() {
        assert_zones(&[
            (87.6168, 43.8256, &["Asia/Shanghai", "Asia/Urumqi"]),
            (75.99, 39.47, &["Asia/Shanghai", "Asia/Urumqi"]),
            (41.0234, 43.0015, &["Asia/Tbilisi", "Europe/Moscow"]),
            (28.4372, 9.5958, &["Africa/Juba", "Africa/Khartoum"]),
            (91.1172, 29.6469, &["Asia/Shanghai"]),
            (43.97, 42.2257, &["Asia/Tbilisi"]),
            (35.2137, 31.7683, &["Asia/Jerusalem"]),
        ]);
    }

    #[test]
    #[cfg(all(feature = "rtree", feature = "s2cell"))]
    fn backends_agree_inland() {
        let names = |resolver: &Resolver, x: f64, y: f64| {
            let found = resolver.search(&point! { x: x, y: y }.bounding_rect());
            let mut names: Vec<String> = found
                .map(|zones| zones.iter().map(|z| z.name().to_string()).collect())
                .unwrap_or_default();
            names.sort_unstable();
            names
        };
        let rtree = RTREE.get().expect("initialised");
        let s2cell = S2CELL.get().expect("initialised");

        // (min lng, min lat, max lng, max lat) of inland areas crossed by many borders.
        let areas = [
            (-104.0, 33.0, -84.0, 46.0),
            (2.0, 44.0, 24.0, 53.0),
            (135.0, -36.0, 150.0, -26.0),
            (30.0, 48.0, 60.0, 60.0),
            (-60.0, -20.0, -45.0, -5.0),
            (75.0, 20.0, 85.0, 28.0),
            (95.0, 25.0, 115.0, 40.0),
        ];
        let (mut total, mut differ) = (0, 0);
        for (x0, y0, x1, y1) in areas {
            for i in 0..200 {
                for j in 0..100 {
                    let x = x0 + (x1 - x0) * (i as f64 + 0.37) / 200.0;
                    let y = y0 + (y1 - y0) * (j as f64 + 0.61) / 100.0;
                    total += 1;
                    differ += usize::from(names(rtree, x, y) != names(s2cell, x, y));
                }
            }
        }
        assert!(differ * 100 < total, "{differ} of {total} points differ");
    }
}
