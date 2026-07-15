use anyhow::{Result, bail};
use geo::{Coord, LineString};

/// A parsed `*_coords.snap` insta snapshot.
pub struct Snapshot {
    /// Raw payload lines (one `"<lon> <lat>"` token per matched point) —
    /// these are what the sequence diff runs over, keeping the diff aligned
    /// with what the snapshot file itself shows.
    pub lines: Vec<String>,
    pub line_string: LineString<f64>,
}

/// Parse an insta snapshot: skip the YAML frontmatter delimited by `---`
/// lines, then read one `lon lat` coordinate per line.
pub fn parse_coords_snap(content: &str) -> Result<Snapshot> {
    let mut lines = content.lines();

    if lines.next().map(str::trim) != Some("---") {
        bail!("not an insta snapshot: missing `---` frontmatter opener");
    }

    for line in lines.by_ref() {
        if line.trim() == "---" {
            let payload: Vec<String> = lines
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_owned())
                .collect();

            let mut coords = Vec::with_capacity(payload.len());
            for (idx, line) in payload.iter().enumerate() {
                let mut parts = line.split_whitespace();
                let (Some(lon), Some(lat)) = (parts.next(), parts.next()) else {
                    bail!("line {}: expected `lon lat`, got {line:?}", idx + 1);
                };
                if parts.next().is_some() {
                    bail!("line {}: trailing tokens in {line:?}", idx + 1);
                }

                coords.push(Coord {
                    x: lon.parse::<f64>()?,
                    y: lat.parse::<f64>()?,
                });
            }

            return Ok(Snapshot {
                lines: payload,
                line_string: LineString::new(coords),
            });
        }
    }

    bail!("not an insta snapshot: frontmatter never closed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_coords() {
        let snap = "---\nsource: benches/map_match.rs\nexpression: coords\n---\n-118.392994 33.950463\n-118.392994 33.950624\n";
        let parsed = parse_coords_snap(snap).unwrap();
        assert_eq!(parsed.lines.len(), 2);
        assert_eq!(parsed.line_string.0[0].x, -118.392994);
        assert_eq!(parsed.line_string.0[1].y, 33.950624);
    }

    #[test]
    fn empty_payload_is_ok() {
        let parsed = parse_coords_snap("---\nsource: x\n---\n").unwrap();
        assert!(parsed.lines.is_empty());
    }

    #[test]
    fn rejects_malformed_lines() {
        assert!(parse_coords_snap("---\nsource: x\n---\nnot-a-coord\n").is_err());
        assert!(parse_coords_snap("---\nsource: x\n---\n-118.0 33.0 5.0\n").is_err());
        assert!(parse_coords_snap("no frontmatter").is_err());
    }

    #[test]
    fn parses_real_snapshots() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benches/snapshots");

        let mut seen = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("_coords.snap")
            {
                continue;
            }
            let content = std::fs::read_to_string(&path).unwrap();
            let parsed = parse_coords_snap(&content).unwrap_or_else(|e| {
                panic!("failed to parse {}: {e}", path.display());
            });
            assert!(
                !parsed.line_string.0.is_empty(),
                "{} was empty",
                path.display()
            );
            seen += 1;
        }
        assert!(seen > 0, "no *_coords.snap fixtures found");
    }
}
