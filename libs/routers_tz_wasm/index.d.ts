// Typings for the flat entry point; see index.js. jco types `option<timestamp>`
// as a required `Timestamp | undefined`, so these declare it optional.
import type {
  BoundingBox,
  Coordinate,
  Polygon,
  Timestamp,
  Timezone,
} from "./dist/transpiled/interfaces/routers-timezone-resolver.js";

export type * from "./dist/transpiled/interfaces/routers-timezone-resolver.js";
export declare function getFromPoint(coordinate: Coordinate, timestamp?: Timestamp): Array<Timezone>;
export declare function batchGetFromPoints(coordinates: Array<Coordinate>, timestamp?: Timestamp): Array<Timezone | undefined>;
export declare function getFromBoundingBox(boundingBox: BoundingBox, timestamp?: Timestamp): Array<Timezone>;
export declare function batchGetFromBoundingBox(boundingBoxes: Array<BoundingBox>, timestamp?: Timestamp): Array<Timezone | undefined>;
export declare function getFromPolygon(polygon: Polygon, timestamp?: Timestamp): Array<Timezone>;
export declare function batchGetFromPolygon(polygons: Array<Polygon>, timestamp?: Timestamp): Array<Timezone | undefined>;
export * as resolver from "./dist/transpiled/interfaces/routers-timezone-resolver.js";
