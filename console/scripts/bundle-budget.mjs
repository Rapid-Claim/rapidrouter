// The console of a microseconds gateway is not allowed to be the slow
// part: total gzipped JS is a CI gate, exactly like the latency budgets.
import { gzipSync } from "node:zlib";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const BUDGET_KB = 250;
const dist = fileURLToPath(new URL("../dist/assets/", import.meta.url));

function totalGzip(extension) {
  let total = 0;
  for (const name of readdirSync(dist)) {
    if (!name.endsWith(extension)) continue;
    total += gzipSync(readFileSync(join(dist, name))).length;
  }
  return total;
}

const js = totalGzip(".js");
const css = totalGzip(".css");
const kb = (bytes) => (bytes / 1024).toFixed(1);

console.log(`JS  ${kb(js)} KB gz  (budget ${BUDGET_KB} KB)`);
console.log(`CSS ${kb(css)} KB gz`);

if (js > BUDGET_KB * 1024) {
  console.error(`\nBundle budget exceeded: ${kb(js)} KB gz > ${BUDGET_KB} KB.`);
  process.exit(1);
}
