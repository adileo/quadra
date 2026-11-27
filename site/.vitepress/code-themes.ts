// The code colours of the site: quiet, few hues. Keywords are bold (ink in the light theme,
// the highlighter yellow in the dark one); types, strings and numbers each get one muted colour.

type Palette = { bg: string; fg: string; kw: string; type: string; fn: string; str: string; num: string; comment: string; punct: string };

function theme(name: string, type: "light" | "dark", p: Palette) {
  return {
    name,
    type,
    colors: { "editor.background": p.bg, "editor.foreground": p.fg },
    tokenColors: [
      { settings: { foreground: p.fg } },
      { scope: ["comment", "punctuation.definition.comment"], settings: { foreground: p.comment } },
      { scope: ["keyword", "storage", "keyword.control", "variable.language"], settings: { foreground: p.kw, fontStyle: "bold" } },
      { scope: ["keyword.operator", "punctuation", "meta.brace", "punctuation.separator.type"], settings: { foreground: p.punct, fontStyle: "" } },
      { scope: ["entity.name.type", "entity.name.class", "support.type", "support.class", "entity.name.section"], settings: { foreground: p.type } },
      { scope: ["entity.name.function", "support.function"], settings: { foreground: p.fn } },
      { scope: ["string", "string.quoted", "string.interpolated"], settings: { foreground: p.str } },
      { scope: ["constant", "constant.numeric", "support.constant", "constant.language"], settings: { foreground: p.num } },
      { scope: ["markup.heading"], settings: { foreground: p.kw, fontStyle: "bold" } },
      { scope: ["markup.inserted"], settings: { foreground: p.fg } },
      { scope: ["markup.deleted"], settings: { foreground: p.comment } },
    ],
  };
}

export const quadraLight = theme("quadra-light", "light", {
  bg: "#f5f5f5",
  fg: "#262626",
  kw: "#171717",
  type: "#0e5a6b",
  fn: "#171717",
  str: "#4d7c0f",
  num: "#b45309",
  comment: "#9a9a9a",
  punct: "#8a8a8a",
});

export const quadraDark = theme("quadra-dark", "dark", {
  bg: "#1c1d20",
  fg: "#d4d4d4",
  kw: "#fde047",
  type: "#93c5fd",
  fn: "#f5f5f5",
  str: "#b9e28c",
  num: "#fdba74",
  comment: "#6b6b6b",
  punct: "#8a8a8a",
});
