// The projects the sandbox can open: site/projects/* (welcome, tutorial steps) and the full
// ERP example, as JSON files in public/projects (one per project, plus an index).
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const site = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const out = path.join(site, "public/projects");
const TEXT = /\.(qd|qdp|toml|md|csv|lock|expected)$/;

function read(root) {
  const files = {};
  const walk = (dir) => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      if (e.name.startsWith(".") || e.name === "node_modules") continue;
      const p = path.join(dir, e.name);
      if (e.isDirectory()) walk(p);
      else if (TEXT.test(e.name)) files[path.relative(root, p).split(path.sep).join("/")] = fs.readFileSync(p, "utf8");
    }
  };
  walk(root);
  return files;
}

function titleOf(name, files) {
  const toml = files["quadra.toml"] ?? "";
  const t = toml.match(/^title\s*=\s*"(.*)"/m);
  if (t) return t[1];
  if (name === "welcome") return "Welcome — a tiny billing app";
  if (name === "erp") return "ERP — the full example";
  const m = name.match(/^tutorial-(\d+)/);
  return m ? `Tutorial · step ${m[1]}` : name;
}

fs.rmSync(out, { recursive: true, force: true });
fs.mkdirSync(out, { recursive: true });
const sources = [];
const dir = path.join(site, "projects");
if (fs.existsSync(dir)) for (const n of fs.readdirSync(dir).sort()) if (fs.statSync(path.join(dir, n)).isDirectory()) sources.push([n, path.join(dir, n)]);
// the full example, from the sources of Quadra around the site
const quadra = path.resolve(site, "..");
sources.push(["erp", path.join(quadra, "examples/extended")]);
// the installers, served at quadra.run/install.sh and /install.ps1, from the same sources
for (const f of ["install.sh", "install.ps1"]) fs.copyFileSync(path.join(quadra, f), path.join(site, "public", f));
const index = [];
for (const [name, root] of sources) {
  const files = read(root);
  const project = { name, title: titleOf(name, files), files };
  fs.writeFileSync(path.join(out, `${name}.json`), JSON.stringify(project));
  index.push({ name, title: project.title, files: Object.keys(files).length });
}
index.sort((a, b) => (a.name === "welcome" ? -1 : b.name === "welcome" ? 1 : a.name === "erp" ? 1 : b.name === "erp" ? -1 : a.name.localeCompare(b.name)));
fs.writeFileSync(path.join(out, "index.json"), JSON.stringify(index, null, 1));
console.log(`sandbox projects: ${index.map((p) => p.name).join(", ")}`);
