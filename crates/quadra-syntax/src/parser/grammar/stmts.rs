//! Statements and statement blocks.

use super::super::Parser;
use super::{at_item, end_line, expr, item, meta_value, name, name_ref, path, recover_line, type_, value};
use crate::syntax_kind::SyntaxKind::*;

/// A block after `:`. Either an indented block on the following lines or a single
/// statement on the same line (`as anna: inv.post()`).
pub(crate) fn block(p: &mut Parser) {
    let m = p.start();
    if p.at(NEWLINE) && p.nth(1) == INDENT {
        p.bump();
        p.bump();
        while !p.at(DEDENT) && !p.at_eof() {
            if p.eat(NEWLINE) {
                continue;
            }
            let before = p.position();
            stmt(p);
            super::ensure_progress(p, before);
        }
        p.eat(DEDENT);
    } else if p.at(NEWLINE) {
        p.error("expected an indented block");
        p.bump();
    } else {
        stmt(p);
    }
    m.complete(p, BLOCK);
}

/// Is the identifier at the cursor used as a statement keyword? It is not when it is
/// immediately used as a value (`expect.x`, `fail = 1`). A following `(` or `[` starts the
/// statement's expression: `expect (a + b) == c`, `expect [1, 2].count() == 2`.
fn kw_position(p: &Parser) -> bool {
    !matches!(p.nth(1), DOT | QUESTION_DOT | EQ | PLUS_EQ | MINUS_EQ | COLON_EQ | NEWLINE | COMMA | R_PAREN)
}

pub(crate) fn stmt(p: &mut Parser) {
    match p.current() {
        LET_KW => return let_stmt(p),
        IF_KW => return if_stmt(p),
        FOR_KW => return for_stmt(p),
        RETURN_KW => {
            let m = p.start();
            p.bump();
            if !p.at(NEWLINE) && !p.at(DEDENT) {
                expr(p);
            }
            end_line(p);
            m.complete(p, RETURN_STMT);
            return;
        }
        AS_KW => {
            let m = p.start();
            p.bump();
            name_ref(p);
            p.expect(COLON);
            block(p);
            m.complete(p, AS_STMT);
            return;
        }
        _ => {}
    }
    if at_item(p) {
        item(p);
        return;
    }
    if p.at(IDENT) {
        let kw = p.nth_text(0);
        if kw_position(p) {
            match kw {
                "require" => {
                    let m = p.start();
                    p.bump_ctx();
                    expr(p);
                    if p.eat(COMMA) {
                        expr(p);
                    }
                    end_line(p);
                    m.complete(p, KEYWORD_STMT);
                    return;
                }
                "fail" | "emit" | "use" | "clock" => {
                    let m = p.start();
                    p.bump_ctx();
                    expr(p);
                    end_line(p);
                    m.complete(p, KEYWORD_STMT);
                    return;
                }
                "expect" if p.nth(1) != COLON => {
                    let m = p.start();
                    p.bump_ctx();
                    expr(p);
                    end_line(p);
                    m.complete(p, KEYWORD_STMT);
                    return;
                }
                "expect_error" | "expect_compile_error" => {
                    let m = p.start();
                    p.bump_ctx();
                    p.expect(STRING);
                    p.expect(COLON);
                    block(p);
                    m.complete(p, EXPECT_BLOCK);
                    return;
                }
                "rename" if p.nth(1) == IDENT => {
                    let m = p.start();
                    p.bump_ctx();
                    path(p);
                    p.expect(ARROW);
                    path(p);
                    end_line(p);
                    m.complete(p, MIGRATION_STEP);
                    return;
                }
                "convert" if p.nth(1) == IDENT => {
                    let m = p.start();
                    p.bump_ctx();
                    path(p);
                    p.expect(FROM_KW);
                    type_(p);
                    p.expect(COLON);
                    super::exprs::arms(p);
                    m.complete(p, MIGRATION_STEP);
                    return;
                }
                "backfill" if p.nth(1) == IDENT => {
                    let m = p.start();
                    p.bump_ctx();
                    path(p);
                    p.expect(COLON);
                    block(p);
                    m.complete(p, MIGRATION_STEP);
                    return;
                }
                _ => {}
            }
        }
        // `verify:` / `expect:` blocks and `reason: "…"` metadata
        if p.nth(1) == COLON {
            let m = p.start();
            if p.nth(2) == NEWLINE && p.nth(3) == INDENT {
                p.bump_ctx();
                p.bump();
                block(p);
                m.complete(p, KEYWORD_STMT);
            } else {
                name(p);
                p.bump();
                meta_value(p);
                end_line(p);
                m.complete(p, META);
            }
            return;
        }
    }
    expr_or_assign(p);
}

fn expr_or_assign(p: &mut Parser) {
    let m = p.start();
    let Some(lhs) = expr(p) else {
        m.abandon(p);
        let e = p.start();
        recover_line(p);
        e.complete(p, ERROR);
        return;
    };
    let _ = lhs;
    if matches!(p.current(), EQ | PLUS_EQ | MINUS_EQ) {
        p.bump();
        expr(p);
        end_line(p);
        m.complete(p, ASSIGN_STMT);
    } else {
        end_line(p);
        m.complete(p, EXPR_STMT);
    }
}

fn let_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();
    name(p);
    if p.eat(COLON) {
        type_(p);
    }
    if p.at(EQ) {
        value(p);
    } else {
        p.error("expected `=`");
    }
    end_line(p);
    m.complete(p, LET_STMT);
}

fn if_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();
    if p.eat(LET_KW) {
        name(p);
        p.expect(EQ);
    }
    expr(p);
    p.expect(COLON);
    block(p);
    if p.at(ELSE_KW) {
        let e = p.start();
        p.bump();
        if p.at(IF_KW) {
            if_stmt(p);
        } else {
            p.expect(COLON);
            block(p);
        }
        e.complete(p, ELSE_BRANCH);
    }
    m.complete(p, IF_STMT);
}

fn for_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump();
    name(p);
    while p.eat(COMMA) {
        name(p);
    }
    p.expect(IN_KW);
    expr(p);
    p.expect(COLON);
    block(p);
    m.complete(p, FOR_STMT);
}
