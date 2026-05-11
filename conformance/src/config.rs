use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub run: RunConfig,
    pub matchers: MatchersConfig,
    #[serde(default)]
    pub traces: Vec<TraceEntry>,
}

#[derive(Debug, Deserialize)]
pub struct RunConfig {
    /// Number of timed iterations per trace (warm-up not included).
    #[serde(default = "default_iterations")]
    pub iterations: usize,
    /// Number of warm-up passes before timing begins (excluded from results).
    #[serde(default = "default_warmup")]
    pub warmup: usize,
    /// Output format: "table" | "json" | "csv"
    #[serde(default = "default_output")]
    pub output: String,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            iterations: default_iterations(),
            warmup: default_warmup(),
            output: default_output(),
        }
    }
}

fn default_iterations() -> usize { 50 }
fn default_warmup() -> usize { 5 }
fn default_output() -> String { "table".to_string() }

#[derive(Debug, Deserialize)]
pub struct MatchersConfig {
    pub routers: Option<RoutersConfig>,
    pub valhalla: Option<ValhallaConfig>,
    pub graphhopper: Option<GraphHopperConfig>,
    pub fmm: Option<FmmConfig>,
}

/// Configuration for the native Routers map matcher.
///
/// The graph is loaded once at startup; only the match call itself is timed,
/// keeping this consistent with the service-level measurement other matchers use
/// (i.e. the server has already loaded its graph before the benchmark starts).
#[derive(Debug, Deserialize)]
pub struct RoutersConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// PBF filename relative to the routers_fixtures resources directory.
    #[serde(default = "default_network")]
    pub network: String,
    #[serde(default = "default_search_distance")]
    pub search_distance: f64,
}

impl Default for RoutersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            network: default_network(),
            search_distance: default_search_distance(),
        }
    }
}

fn default_network() -> String { "los-angeles-minified.osm.pbf".to_string() }
fn default_search_distance() -> f64 { 50.0 }

/// Valhalla map matching via its `/trace_route` HTTP endpoint.
///
/// Use `shape_match = "map_snap"` (Valhalla's recommended mode for raw GPS traces)
/// and keep `gps_accuracy` at the Valhalla default of 5 m so that the service
/// configuration is unchanged from its out-of-the-box behaviour.
#[derive(Debug, Deserialize)]
pub struct ValhallaConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_valhalla_url")]
    pub url: String,
    /// Routing cost model.  "auto" is the closest equivalent to a generic
    /// car-mode match, matching the GraphHopper and Routers defaults.
    #[serde(default = "default_valhalla_costing")]
    pub costing: String,
    /// "map_snap" lets Valhalla freely snap each point to the best candidate;
    /// this is the recommended mode for noisy GPS traces.
    #[serde(default = "default_shape_match")]
    pub shape_match: String,
    /// Valhalla's default GPS accuracy value (metres).
    #[serde(default = "default_gps_accuracy")]
    pub gps_accuracy: u32,
}

fn default_valhalla_url() -> String { "http://localhost:8002".to_string() }
fn default_valhalla_costing() -> String { "auto".to_string() }
fn default_shape_match() -> String { "map_snap".to_string() }
fn default_gps_accuracy() -> u32 { 5 }

/// GraphHopper map matching via its `/match` HTTP endpoint.
///
/// The request body is GPX XML (GraphHopper's native format) with the profile
/// and accuracy passed as query parameters.  Using the "car" profile mirrors the
/// Valhalla "auto" costing used above.
#[derive(Debug, Deserialize)]
pub struct GraphHopperConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_gh_url")]
    pub url: String,
    #[serde(default = "default_gh_profile")]
    pub profile: String,
    #[serde(default = "default_gh_gps_accuracy")]
    pub gps_accuracy: u32,
}

fn default_gh_url() -> String { "http://localhost:8989".to_string() }
fn default_gh_profile() -> String { "car".to_string() }
fn default_gh_gps_accuracy() -> u32 { 5 }

/// FMM (Fast Map Matching) via its C++ HTTP service.
///
/// The service is a thin cpp-httplib wrapper around the FMM C++ library;
/// see `fmm_server/` for the source and `docker/` for the Dockerfile.
/// The road network shapefile is in WGS84 (EPSG:4326), so radius and error
/// are in degrees (~0.003° ≈ 300 m, ~0.0005° ≈ 50 m at LA latitude).
#[derive(Debug, Deserialize)]
pub struct FmmConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_fmm_url")]
    pub url: String,
    /// Number of candidate edges per GPS point (FMM k-nearest).
    #[serde(default = "default_fmm_k")]
    pub k: u32,
    /// Candidate search radius in degrees (network is WGS84).
    #[serde(default = "default_fmm_radius")]
    pub radius: f64,
    /// Assumed GPS measurement error in degrees (network is WGS84).
    #[serde(default = "default_fmm_error")]
    pub error: f64,
}

fn default_fmm_url() -> String { "http://localhost:9090".to_string() }
fn default_fmm_k() -> u32 { 8 }
fn default_fmm_radius() -> f64 { 0.003 }
fn default_fmm_error() -> f64 { 0.0005 }

fn default_enabled() -> bool { true }

#[derive(Debug, Deserialize, Clone)]
pub struct TraceEntry {
    pub id: String,
    pub file: String,
}
