// Flat entry point: hoists the `resolver` interface's members to top-level exports.
import { resolver } from "./dist/transpiled/routers_tz_wasm.js";

export const {
  getFromPoint,
  batchGetFromPoints,
  getFromBoundingBox,
  batchGetFromBoundingBox,
  getFromPolygon,
  batchGetFromPolygon,
} = resolver;
export { resolver };
