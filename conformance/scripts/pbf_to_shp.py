#!/usr/bin/env python3
"""
Convert a GeoJSON road network (produced by osmium export) to an
FMM-compatible ESRI Shapefile with integer id / source / target fields.

Source/target node IDs are derived from rounded endpoint coordinates so
that topologically shared points get the same ID.

Usage:
  pbf_to_shp.py <input.geojson> <output.shp>
or via env vars GEOJSON_IN / SHP_OUT.
"""
import json
import os
import sys
from osgeo import ogr, osr


def main():
    geojson_path = os.environ.get("GEOJSON_IN") or (sys.argv[1] if len(sys.argv) > 1 else None)
    shp_path     = os.environ.get("SHP_OUT")     or (sys.argv[2] if len(sys.argv) > 2 else None)

    if not geojson_path or not shp_path:
        sys.exit("Usage: pbf_to_shp.py <input.geojson> <output.shp>")

    driver = ogr.GetDriverByName("ESRI Shapefile")
    if os.path.exists(shp_path):
        driver.DeleteDataSource(shp_path)

    ds    = driver.CreateDataSource(shp_path)
    srs   = osr.SpatialReference()
    srs.ImportFromEPSG(4326)
    layer = ds.CreateLayer("roads", srs=srs, geom_type=ogr.wkbLineString)

    for name in ("id", "source", "target"):
        layer.CreateField(ogr.FieldDefn(name, ogr.OFTInteger64))

    defn  = layer.GetLayerDefn()
    nodes = {}  # (rounded_lon, rounded_lat) -> node_id

    def node_id(coord):
        key = (round(coord[0], 7), round(coord[1], 7))
        if key not in nodes:
            nodes[key] = len(nodes)
        return nodes[key]

    # Accept both regular GeoJSON and newline-delimited GeoJSON (geojsonl).
    def iter_features(path):
        with open(path) as f:
            first_char = f.read(1)
            f.seek(0)
            if first_char == "{":
                # Try full GeoJSON first, fall back to ndjson
                content = f.read()
                try:
                    obj = json.loads(content)
                    if obj.get("type") == "FeatureCollection":
                        yield from obj.get("features", [])
                        return
                    if obj.get("type") == "Feature":
                        yield obj
                        return
                except json.JSONDecodeError:
                    pass
                # Newline-delimited
                for line in content.splitlines():
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        o = json.loads(line)
                        if o.get("type") == "Feature":
                            yield o
                    except json.JSONDecodeError:
                        pass

    edge_id = 0
    for feat in iter_features(geojson_path):
        geom_json = feat.get("geometry", {})
        if geom_json.get("type") != "LineString":
            continue
        coords = geom_json["coordinates"]
        if len(coords) < 2:
            continue

        src = node_id(coords[0])
        tgt = node_id(coords[-1])

        geom     = ogr.CreateGeometryFromJson(json.dumps(geom_json))
        ogr_feat = ogr.Feature(defn)
        ogr_feat.SetGeometry(geom)
        ogr_feat.SetField("id",     edge_id)
        ogr_feat.SetField("source", src)
        ogr_feat.SetField("target", tgt)
        layer.CreateFeature(ogr_feat)
        edge_id += 1

    ds.Destroy()
    print(f"[pbf_to_shp] {edge_id} edges, {len(nodes)} nodes → {shp_path}")


if __name__ == "__main__":
    main()
