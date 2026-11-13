// Builds the dashboard SPA into web/dist (checked in and embedded in the
// binary by crates/pmt-web). `node build.mjs --check` rebuilds in memory
// and fails when web/dist differs, so the embedded SPA always matches its
// sources (scripts/check runs it).
import { build } from "esbuild";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, "dist");
const check = process.argv.includes("--check");

const result = await build({
  entryPoints: { app: join(here, "src/main.ts") },
  bundle: true,
  format: "esm",
  target: "es2022",
  minify: true,
  legalComments: "none",
  charset: "utf8",
  write: false,
  outdir: dist,
  logLevel: "warning",
});

const outputs = new Map(result.outputFiles.map((f) => [f.path.split(/[\\/]/).pop(), f.contents]));
outputs.set("app.css", readFileSync(join(here, "src/styles.css")));
outputs.set("index.html", readFileSync(join(here, "src/index.html")));

let stale = [];
for (const [name, bytes] of outputs) {
  const path = join(dist, name);
  if (check) {
    let current;
    try {
      current = readFileSync(path);
    } catch {
      current = Buffer.alloc(0);
    }
    if (!Buffer.from(bytes).equals(current)) stale.push(name);
  } else {
    mkdirSync(dist, { recursive: true });
    writeFileSync(path, bytes);
  }
}
if (stale.length > 0) {
  console.error(`web/dist is stale (${stale.join(", ")}); run: (cd web && npm run build)`);
  process.exit(1);
}
