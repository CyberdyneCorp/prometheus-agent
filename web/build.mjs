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

/** Bundles `entry` and returns the files of its dist, by name; `assets`
 *  are copied as they are (published name -> source). */
async function bundle(entry, dist, styles, html, scripts = {}, assets = {}) {
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
  // Scripts injected into other documents: one classic script each, its
  // names kept out of the page's globals.
  for (const [name, path] of Object.entries(scripts)) {
    const script = await build({ entryPoints: [path], bundle: true, format: "iife", target: "es2022", minify: true, legalComments: "none", charset: "utf8", write: false, logLevel: "warning" });
    outputs.set(`${name}.js`, script.outputFiles[0].contents);
  }
  for (const [name, path] of Object.entries(assets)) outputs.set(name, readFileSync(path));
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
      mkdirSync(dirname(path), { recursive: true });
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
    // Installed by the shell in every frame (the preview's annotate agent).
    { annotate: join(desktop, "src/annotate-agent.ts") },
    // The terminal pane's bundled monospace font (SIL OFL 1.1, licence beside it).
    Object.fromEntries(
      ["jetbrains-mono-latin-400-normal.woff2", "jetbrains-mono-latin-ext-400-normal.woff2", "JetBrainsMono-OFL.txt"].map((f) => [`fonts/${f}`, join(desktop, "src/fonts", f)]),
    ),
  ),
  "apps/desktop/dist",
);
if (!web || !app) process.exit(1);
