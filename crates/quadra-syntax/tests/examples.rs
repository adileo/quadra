//! The example projects are the conformance suite: every file must parse without errors
//! and the tree must reproduce the source exactly.

use std::path::{Path, PathBuf};

fn qd_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            qd_files(&path, out);
        } else if path.extension().map_or(false, |e| e == "qd") {
            out.push(path);
        }
    }
}

#[test]
fn examples_parse_without_errors() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut files = Vec::new();
    qd_files(&root, &mut files);
    qd_files(&root.join("../std"), &mut files);
    files.sort();
    assert!(
        files.len() >= 15,
        "examples not found in {}",
        root.display()
    );

    let mut failures = String::new();
    for file in &files {
        let src = std::fs::read_to_string(file).unwrap();
        let parse = quadra_syntax::parse(&src);
        assert_eq!(
            parse.syntax().to_string(),
            src,
            "tree is not lossless for {}",
            file.display()
        );
        for e in &parse.errors {
            let line = src[..usize::from(e.range.start())].matches('\n').count() + 1;
            failures.push_str(&format!("{}:{}: {}\n", file.display(), line, e.message));
        }
    }
    assert!(failures.is_empty(), "parse errors:\n{failures}");
}

#[test]
fn reports_useful_errors() {
    let cases = [
        ("entity A\n    x: Int\n", "expected `:`"),
        ("entity A:\n\tx: Int\n", "tabs are not allowed"),
        (
            "entity A:\n    x: Int\n  y: Int\n",
            "indentation does not match",
        ),
        ("entity A:\n    x: \n", "expected a type"),
        ("frobnicate\n", "expected a declaration"),
        ("fn f():\n    let x = \n", "expected an expression"),
    ];
    for (src, expected) in cases {
        let parse = quadra_syntax::parse(src);
        assert!(
            parse.errors.iter().any(|e| e.message.contains(expected)),
            "{src:?}: expected an error containing {expected:?}, got {:?}",
            parse.errors
        );
        assert_eq!(parse.syntax().to_string(), src);
    }
}

/// Parse without errors and return the operators of the outermost binary expression of the
/// first `const` (or statement), outside-in.
fn ops_of(src: &str) -> Vec<String> {
    use quadra_syntax::SyntaxKind;
    let parse = quadra_syntax::parse(src);
    assert!(parse.errors.is_empty(), "{src:?}: {:?}", parse.errors);
    let mut ops = Vec::new();
    let mut node = parse
        .syntax()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::BIN_EXPR);
    while let Some(n) = node {
        let op = n
            .children_with_tokens()
            .filter_map(|e| e.into_token())
            .find(|t| !t.kind().is_trivia())
            .map(|t| t.text().to_string())
            .unwrap_or_default();
        ops.push(op);
        // descend into the left operand when it is a binary expression
        node = n
            .children()
            .next()
            .filter(|c| c.kind() == SyntaxKind::BIN_EXPR);
    }
    ops
}

#[test]
fn else_if_on_one_line() {
    // issue 19
    let src = "fn f(x: Int) -> Int:\n    if x > 0:\n        return 1\n    else if x < 0:\n        return -1\n    else:\n        return 0\n";
    let parse = quadra_syntax::parse(src);
    assert!(parse.errors.is_empty(), "{:?}", parse.errors);
    assert_eq!(parse.syntax().to_string(), src);
    let tree = parse.debug_tree();
    assert_eq!(tree.matches("IF_STMT@").count(), 2, "{tree}");
    // a ternary continued on the next line still works
    let src = "const a = 1 if true\n    else 2\n";
    assert!(quadra_syntax::parse(src).errors.is_empty());
}

#[test]
fn statement_keyword_before_parenthesis() {
    // issue 20
    for src in [
        "test \"t\":\n    expect (1 + 1) == 2\n",
        "test \"t\":\n    expect [1, 2].count() == 2\n",
        "fn f(x: Int):\n    require (x) > 0, \"positive\"\n",
        "fn f(x: Int):\n    fail (\"no\")\n",
    ] {
        let parse = quadra_syntax::parse(src);
        assert!(parse.errors.is_empty(), "{src:?}: {:?}", parse.errors);
        let tree = parse.debug_tree();
        let stmt = tree
            .lines()
            .find(|l| l.contains("_STMT@"))
            .unwrap_or_default();
        assert!(stmt.contains("KEYWORD_STMT@"), "{src:?}:\n{tree}");
    }
}

#[test]
fn coalesce_binds_tighter_than_comparison() {
    // issue 21: `a ?? b == c` is `(a ?? b) == c`
    assert_eq!(ops_of("const x = a ?? b == c\n"), ["==", "??"]);
    assert_eq!(ops_of("const x = c == a ?? b\n"), ["=="]);
    assert_eq!(ops_of("const x = a ?? b and c\n"), ["and", "??"]);
    assert_eq!(ops_of("const x = a ?? b + 1\n"), ["??"]);
    assert_eq!(ops_of("const x = a + 1 ?? b\n"), ["??", "+"]);
    // right-associative
    assert_eq!(ops_of("const x = a ?? b ?? c\n"), ["??"]);
}
