import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig, type DefaultTheme } from "vitepress";
import { quadraDark, quadraLight } from "./code-themes";

const here = path.dirname(fileURLToPath(import.meta.url));
const site = path.resolve(here, "..");

// The highlighting of the editor extension: one grammar for VS Code, Cursor and the site.
// the sources of Quadra (this repository, around the site): the grammar of the editor extension
// colours the code of the documentation
const quadra = path.resolve(site, "..");
const grammar = JSON.parse(fs.readFileSync(path.join(quadra, "editors/vscode/syntaxes/quadra.tmLanguage.json"), "utf8"));

/** The title of a page: its first `# heading` (or the file name). */
function titleOf(file: string): string {
  const text = fs.readFileSync(file, "utf8");
  const fm = text.match(/^---\n[\s\S]*?\ntitle:\s*(.+)\n[\s\S]*?---/);
  if (fm) return fm[1].replace(/^["']|["']$/g, "");
  const h = text.match(/^#\s+(.+)$/m);
  return h ? h[1].trim() : path.basename(file, ".md");
}

/** The pages of a section, in a given order; pages not listed follow, by name. */
function pages(dir: string, order: string[] = []): DefaultTheme.SidebarItem[] {
  const abs = path.resolve(site, dir);
  if (!fs.existsSync(abs)) return [];
  const names = fs
    .readdirSync(abs)
    .filter((f) => f.endsWith(".md") && f !== "index.md")
    .map((f) => f.replace(/\.md$/, ""));
  names.sort((a, b) => {
    const ia = order.indexOf(a);
    const ib = order.indexOf(b);
    return (ia < 0 ? 999 : ia) - (ib < 0 ? 999 : ib) || a.localeCompare(b);
  });
  return names.map((n) => ({ text: titleOf(path.join(abs, `${n}.md`)).replace(/^Step \d+:\s*/i, ""), link: `/${dir}/${n}` }));
}

const guide = pages("guide", ["introduction", "installation", "quick-start", "what-quadra-is-not"]);
const tutorial = pages("tutorial").map((p, i) => ({ ...p, text: `${String(i + 1).padStart(2, "0")} · ${p.text.replace(/^\d+\s*[·.:-]\s*/, "")}` }));
const reference = pages("reference", [
  "projects-and-modules",
  "syntax",
  "entities",
  "types",
  "expressions",
  "rules",
  "flows-and-actions",
  "connections",
  "documents",
  "ledgers",
  "security",
  "testing",
  "migrations-and-changes",
  "metrics",
  "views-and-boards",
  "apps-and-dashboards",
  "interfaces",
  "cli",
  "http-api",
  "storage",
  "retention",
]);

const hostname = "https://quadra.run";

/** The first paragraph of a page, as one plain line: its summary in llms.txt. */
function summaryOf(file: string): string {
  const text = fs.readFileSync(file, "utf8");
  const described = text.match(/^---\n[\s\S]*?\ndescription:\s*(.+)\n[\s\S]*?---/);
  if (described) return described[1].replace(/^["']|["']$/g, "").trim();
  const body = text.replace(/^---\n[\s\S]*?\n---\n/, "");
  const after = body.split(/^#\s+.+$/m)[1] ?? body;
  const para = after.split(/\n\s*\n/).map((p) => p.trim()).find((p) => p && !/^[<|`:#>!-]/.test(p)) ?? "";
  const line = para.replace(/\[([^\]]+)\]\([^)]+\)/g, "$1").replace(/[*`]/g, "").replace(/\s+/g, " ").trim();
  return line.length > 220 ? line.slice(0, 217).replace(/\s+\S*$/, "") + "…" : line;
}

/** llms.txt (an index for AI agents) and llms-full.txt (every page of the docs, as Markdown). */
function writeLlms(outDir: string) {
  const sections: [string, string[]][] = [
    ["Start here", ["/guide/introduction", "/guide/installation", "/guide/quick-start", "/guide/what-quadra-is-not", "/safe-by-design", "/compliance"]],
    ["Tutorial", ["/tutorial/", ...tutorial.map((p) => p.link!)]],
    ["Reference", ["/reference/", ...reference.map((p) => p.link!)]],
  ];
  const fileOf = (link: string) => path.join(site, link.endsWith("/") ? `${link}index.md` : `${link}.md`);
  const index = [
    "# Quadra",
    "",
    "> Quadra is a programming language for finance and operations: the rules of a business written as code (entities, formulas, rules, flows, ledgers, permissions), from which the engine builds the database, the API and the app. Rules are checked on every entry, every number is traceable and every change is on record. The engine is open source; Quadra Studio is the app built from the code.",
    "",
    "Files are `.qd` (the model) and `.qdi` (interfaces). `quadra check` checks a project, `quadra test` runs its tests, `quadra dev` runs it with Quadra Studio. The full documentation as one file: " + hostname + "/llms-full.txt",
    "",
  ];
  const full: string[] = [];
  for (const [name, links] of sections) {
    index.push(`## ${name}`, "");
    for (const link of links) {
      const file = fileOf(link);
      if (!fs.existsSync(file)) continue;
      const summary = summaryOf(file);
      index.push(`- [${titleOf(file).replace(/^Step \d+:\s*/i, "")}](${hostname}${link})${summary ? `: ${summary}` : ""}`);
      const md = fs.readFileSync(file, "utf8").replace(/^---\n[\s\S]*?\n---\n/, "").trim();
      full.push(`<!-- ${hostname}${link} -->\n\n${md}`);
    }
    index.push("");
  }
  index.push("## Optional", "", `- [Sandbox](${hostname}/sandbox): write Quadra in the browser and run it`, `- [Source code](https://github.com/adileo/quadra)`, `- [Quadra Cloud](https://app.quadra.run)`, "");
  fs.writeFileSync(path.join(outDir, "llms.txt"), index.join("\n"));
  fs.writeFileSync(path.join(outDir, "llms-full.txt"), full.join("\n\n---\n\n") + "\n");
}

const sidebarDocs: DefaultTheme.SidebarItem[] = [
  { text: "Getting started", items: guide },
  { text: "Tutorial", link: "/tutorial/", collapsed: false, items: tutorial },
  { text: "Reference", link: "/reference/", collapsed: true, items: reference },
];

export default defineConfig({
  title: "Quadra",
  titleTemplate: ":title · Quadra",
  description: "Programming language for finance and operations. As easy as a spreadsheet, more solid than an ERP: rules checked on every entry, every number traceable, every change on record.",
  cleanUrls: true,
  sitemap: { hostname },
  buildEnd: (config) => writeLlms(config.outDir),
  srcExclude: ["README.md", "projects/**", "scripts/**", "archive/**"],
  lang: "en",
  appearance: true,
  head: [
    ["link", { rel: "icon", type: "image/svg+xml", href: "/logo.svg" }],
    ["link", { rel: "preconnect", href: "https://fonts.googleapis.com" }],
    ["link", { rel: "preconnect", href: "https://fonts.gstatic.com", crossorigin: "" }],
    ["link", { rel: "stylesheet", href: "https://fonts.googleapis.com/css2?family=Geist:wght@400;500;600;700&family=Geist+Mono:wght@400;500;600;700&display=swap" }],
    ["meta", { name: "theme-color", content: "#171717" }],
    ["meta", { property: "og:title", content: "Quadra: programming language for finance and operations" }],
  ],
  markdown: {
    languages: [{ ...grammar, name: "quadra", aliases: ["qd"] }],
    theme: { light: quadraLight, dark: quadraDark },
  },
  themeConfig: {
    logo: { light: "/logo.svg", dark: "/logo-dark.svg" },
    siteTitle: "Quadra",
    nav: [
      { text: "Guide", link: "/guide/introduction", activeMatch: "^/guide/" },
      { text: "Tutorial", link: "/tutorial/", activeMatch: "^/tutorial/" },
      { text: "Reference", link: "/reference/", activeMatch: "^/reference/" },
      { text: "Sandbox", link: "/sandbox" },
      { text: "Cloud", link: "https://app.quadra.run", target: "_self", rel: "noopener" },
    ],
    sidebar: {
      "/guide/": sidebarDocs,
      "/tutorial/": sidebarDocs,
      "/reference/": [
        { text: "Getting started", collapsed: true, items: guide },
        { text: "Tutorial", link: "/tutorial/", collapsed: true, items: tutorial },
        { text: "Reference", link: "/reference/", items: reference },
      ],
    },
    outline: { level: [2, 3] },
    search: { provider: "local" },
    docFooter: { prev: "Previous", next: "Next" },
    footer: { message: "Quadra: the books balance.", copyright: "" },
  },
  vite: {
    server: { fs: { allow: [path.resolve(site, ".."), quadra] } },
    resolve: { dedupe: ["@codemirror/state", "@codemirror/view", "@codemirror/language"] },
  },
});
