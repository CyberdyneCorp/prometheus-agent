// Builds the dashboard SPA into web/dist (checked in and embedded in the
// binary by crates/pmt-web) and the desktop app's page into
// apps/desktop/dist (checked in and embedded in the desktop app by
// Tauri), which reuses the dashboard's components. `node build.mjs
// --check` rebuilds in memory and fails when either dist differs, so what
// is embedded always matches its sources (scripts/check runs it).
import { build } from "esbuild";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const check = process.argv.includes("--check");

/** Bundles `entry` and returns the files of its dist, by name. */
async function bundle(entry, dist, styles, html) {
  const result = await build({
    entryPoints: { app: entry },
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
  outputs.set("app.css", Buffer.concat(styles.map((s) => readFileSync(s))));
  outputs.set("index.html", readFileSync(html));
  return outputs;
}

/** Writes `outputs` into `dist`, or with --check lists what differs. */
function emit(dist, outputs, label) {
  const stale = [];
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
  if (stale.length > 0) console.error(`${label} is stale (${stale.join(", ")}); run: (cd web && npm run build)`);
  return stale.length === 0;
}

const desktop = join(here, "../apps/desktop");
const web = emit(
  join(here, "dist"),
  await bundle(join(here, "src/main.ts"), join(here, "dist"), [join(here, "src/styles.css")], join(here, "src/index.html")),
  "web/dist",
);
const app = emit(
  join(desktop, "dist"),
  await bundle(
    join(desktop, "src/main.ts"),
    join(desktop, "dist"),
    [join(here, "src/styles.css"), join(desktop, "src/desktop.css")],
    join(desktop, "src/index.html"),
  ),
  "apps/desktop/dist",
);
if (!web || !app) process.exit(1);
