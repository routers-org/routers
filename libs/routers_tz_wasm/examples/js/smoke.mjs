// Publish gate for the npm package: the transpiled module must load, its wasm
// core must instantiate, and the resolver must place Sydney and Canberra. Run
// after `just wasm-transpile` (and `npm install` for the WASI shim):
//   node libs/routers_tz_wasm/examples/js/smoke.mjs

import { getFromPoint, batchGetFromPoints } from "../../index.js";

const sydney = { latitude: -33.89, longitude: 151.19 };
const canberra = { latitude: -35.28, longitude: 149.13 };

const [zone] = getFromPoint(sydney);
const [batched] = batchGetFromPoints([canberra]);
if (zone?.ianaCode !== "Australia/Sydney" || batched?.ianaCode !== "Australia/Sydney") {
  console.error(`bad timezone: point=${zone?.ianaCode}, batch=${batched?.ianaCode}`);
  process.exit(1);
}

console.log(`smoke ok: ${sydney.latitude},${sydney.longitude} -> ${zone.ianaCode} (dst: ${zone.isDst})`);
