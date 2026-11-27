// Quadra in CodeMirror (a copy of web/src/code/quadra.ts of quadra-studio: keep them in sync).

import { autocompletion, type CompletionContext, type CompletionResult } from "@codemirror/autocomplete";
import { HighlightStyle, StreamLanguage, getIndentUnit, indentService, indentUnit, syntaxHighlighting } from "@codemirror/language";
import { EditorView } from "@codemirror/view";
import { tags as t } from "@lezer/highlight";

const KEYWORDS = new Set(
  (
    "entity record trait enum extend patch ledger event fn builtin action const role view report metric control migration change fixture test seed " +
    "import from as if else elif for in while return require fail emit expect expect_error expect_compile_error use clock let match when where default unique " +
    "required per frozen except live flow by post to on enters becomes converges before key measure attr is not and or implies " +
    "read edit create run deny allow hide history all rename convert backfill verify temporary load parent via through"
  ).split(" "),
);
const ATOMS = new Set(["true", "false", "none", "self", "me", "current", "old"]);
const DECLARES = new Set(["entity", "record", "trait", "enum", "ledger", "event", "fn", "action", "const", "role", "view", "report", "metric", "control", "migration", "change", "fixture", "extend", "patch"]);

type State = { afterDot: boolean; declare: boolean; afterNumber: boolean };

const language = StreamLanguage.define<State>({
  name: "quadra",
  startState: () => ({ afterDot: false, declare: false, afterNumber: false }),
  token(stream, state) {
    if (stream.sol()) {
      state.afterNumber = false;
      state.afterDot = false;
    }
    if (stream.eatSpace()) return null;
    const afterNumber = state.afterNumber;
    state.afterNumber = false;
    if (stream.match(/#.*/)) return "comment";
    if (stream.match(/f?"(?:[^"\\]|\\.)*"?/)) {
      state.afterDot = false;
      return "string";
    }
    if (stream.match(/\d{4}-\d{2}-\d{2}(T\d{2}:\d{2})?/)) return "number";
    if (stream.match(/\d[\d_]*(\.\d+)?%?/)) {
      state.afterNumber = true;
      return "number";
    }
    if (stream.match(/=>|->|\?\.|\?\?|==|!=|<=|>=|\.\.|[-+*/<>=!?]/)) {
      state.afterDot = false;
      return "operator";
    }
    if (stream.match(/[A-Za-z_][A-Za-z0-9_]*/)) {
      const w = stream.current();
      const wasDot = state.afterDot;
      state.afterDot = false;
      if (state.declare) {
        state.declare = false;
        return "def";
      }
      if (afterNumber) return "unit";
      if (wasDot) return "propertyName";
      if (KEYWORDS.has(w)) {
        if (DECLARES.has(w)) state.declare = true;
        return "keyword";
      }
      if (ATOMS.has(w)) return "atom";
      if (/^[A-Z]/.test(w)) return "typeName";
      if (stream.peek() === "(") return "function";
      return "variableName";
    }
    if (stream.eat(".")) {
      state.afterDot = true;
      return "punctuation";
    }
    stream.next();
    state.afterDot = false;
    return "punctuation";
  },
  languageData: { commentTokens: { line: "#" }, indentOnInput: /^\s*(else|elif)\b/ },
  tokenTable: { unit: t.unit, function: t.function(t.variableName), def: t.definition(t.typeName) },
});

// Indentation like Python: keep the previous line's, one more after a line ending with `:`,
// one less for `else`/`elif`.
const indent = indentService.of((cx, pos) => {
  const unit = getIndentUnit(cx.state);
  const after = cx.lineAt(pos, 1).text.slice(pos - cx.lineAt(pos, 1).from).trimStart();
  let p = pos - 1;
  while (p >= 0) {
    const line = cx.lineAt(p, -1);
    const code = line.text.replace(/#.*$/, "").trimEnd();
    if (code.trim()) {
      let col = cx.lineIndent(line.from, -1);
      if (code.endsWith(":")) col += unit;
      if (/^(else|elif)\b/.test(after)) col -= unit;
      return Math.max(0, col);
    }
    p = line.from - 1;
  }
  return 0;
});

const highlight = HighlightStyle.define([
  { tag: t.keyword, color: "#fde047", fontWeight: "600" },
  { tag: t.comment, color: "#6b6b6b" },
  { tag: t.string, color: "#b9e28c" },
  { tag: t.number, color: "#fdba74" },
  { tag: t.unit, color: "#fdba74" },
  { tag: t.atom, color: "#fdba74" },
  { tag: t.typeName, color: "#93c5fd" },
  { tag: t.definition(t.typeName), color: "#93c5fd", fontWeight: "600" },
  { tag: t.propertyName, color: "#e5e5e5" },
  { tag: t.function(t.variableName), color: "#f5f5f5" },
  { tag: t.variableName, color: "#d4d4d4" },
  { tag: t.operator, color: "#a3a3a3" },
  { tag: t.punctuation, color: "#8a8a8a" },
]);

const theme = EditorView.theme(
  {
    "&": { color: "#e5e5e5", backgroundColor: "#17181b", height: "100%", fontSize: "13px" },
    ".cm-scroller": { fontFamily: "'Geist Mono', 'SF Mono', ui-monospace, Menlo, monospace", lineHeight: "1.65" },
    ".cm-content": { caretColor: "#fde047", padding: "12px 0" },
    ".cm-cursor, .cm-dropCursor": { borderLeftColor: "#fde047", borderLeftWidth: "2px" },
    "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection": { backgroundColor: "#3a3a2a !important" },
    ".cm-activeLine": { backgroundColor: "#1e1f22" },
    ".cm-gutters": { backgroundColor: "#17181b", color: "#4a4a4a", border: "none", paddingRight: "8px" },
    ".cm-activeLineGutter": { backgroundColor: "#1e1f22", color: "#a3a3a3" },
    ".cm-foldPlaceholder": { backgroundColor: "#2a2b2f", border: "none", color: "#a3a3a3" },
    ".cm-matchingBracket": { backgroundColor: "#34353a", outline: "1px solid #5c5c5c" },
    ".cm-searchMatch": { backgroundColor: "#5b4a1d" },
    ".cm-searchMatch.cm-searchMatch-selected": { backgroundColor: "#8a6d1f" },
    ".cm-tooltip": { backgroundColor: "#1e1f22", border: "1px solid #2e2f33", color: "#e5e5e5", borderRadius: "8px", overflow: "hidden" },
    ".cm-tooltip-autocomplete > ul > li[aria-selected]": { backgroundColor: "#3a3a2a", color: "#fff" },
    ".cm-completionDetail": { color: "#8a8a8a", fontStyle: "normal", marginLeft: "8px" },
    ".cm-diagnostic": { padding: "6px 10px", borderLeft: "3px solid" },
    ".cm-diagnostic-error": { borderLeftColor: "#f87171" },
    ".cm-diagnostic-warning": { borderLeftColor: "#fde047" },
    ".cm-lintRange-error": { backgroundImage: "none", textDecoration: "underline wavy #f87171", textUnderlineOffset: "3px" },
    ".cm-lintRange-warning": { backgroundImage: "none", textDecoration: "underline wavy #fde047", textUnderlineOffset: "3px" },
    ".cm-panels": { backgroundColor: "#1e1f22", color: "#e5e5e5", borderColor: "#2e2f33" },
    ".cm-tooltip-hover": { maxWidth: "560px" },
    ".q-hover": { padding: "8px 12px", fontSize: "12.5px", lineHeight: "1.55" },
    ".q-hover pre": { margin: "0 0 6px", padding: "6px 8px", background: "#17181b", borderRadius: "6px", color: "#d4d4d4", whiteSpace: "pre-wrap", fontFamily: "'Geist Mono', ui-monospace, monospace", fontSize: "12px" },
    ".q-hover code": { background: "#17181b", padding: "0 4px", borderRadius: "4px", color: "#ededed", fontFamily: "'Geist Mono', ui-monospace, monospace", fontSize: "12px" },
    ".q-hover hr": { border: "none", borderTop: "1px solid #2e2f33", margin: "6px 0" },
    ".q-hover li": { listStyle: "none", paddingLeft: "2px" },
    "&.q-cmd .cm-content .q-link": { textDecoration: "underline", cursor: "pointer", color: "#fde047" },
    ".cm-completionIcon": { opacity: "0.9", width: "1.1em" },
    ".cm-completionLabel": { fontFamily: "'Geist Mono', ui-monospace, monospace" },
    ".cm-panels input, .cm-panels button": { backgroundColor: "#17181b", color: "#e5e5e5", border: "1px solid #2e2f33", borderRadius: "4px" },
  },
  { dark: true },
);

/** Completion comes from the server, which knows the type before the dot. */
export type Complete = (ctx: CompletionContext) => Promise<CompletionResult | null>;

export function quadra(complete: Complete) {
  return [language, indent, syntaxHighlighting(highlight), theme, indentUnit.of("    "), autocompletion({ override: [complete], activateOnTypingDelay: 80, icons: true })];
}
