import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig, type DefaultTheme } from "vitepress";

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

const guide = pages("guide", ["introduction", "installation", "quick-start"]);
const tutorial = pages("tutorial").map((p, i) => ({ ...p, text: `${String(i + 1).padStart(2, "0")} · ${p.text.replace(/^\d+\s*[·.:-]\s*/, "")}` }));
const reference = pages("reference", [
  "projects-and-modules",
  "syntax",
  "entities",
  "types",
  "expressions",
  "rules",
  "flows-and-actions",
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
]);

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
  srcExclude: ["README.md", "projects/**", "scripts/**", "archive/**"],
  lang: "en",
  appearance: "dark",
  head: [
    ["link", { rel: "icon", type: "image/svg+xml", href: "/logo.svg" }],
    ["meta", { name: "theme-color", content: "#7c3aed" }],
    ["meta", { property: "og:title", content: "Quadra: programming language for finance and operations" }],
  ],
  markdown: {
    languages: [{ ...grammar, name: "quadra", aliases: ["qd"] }],
    theme: { light: "github-light", dark: "one-dark-pro" },
  },
  themeConfig: {
    logo: "/logo.svg",
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
