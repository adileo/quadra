//! Expressions: Pratt parser. Precedence table in the syntax reference (quadra.run/reference/syntax) §5.

use super::super::{CompletedMarker, Parser};
use super::{end_line, name, name_ref, stmts};
use crate::syntax_kind::SyntaxKind::*;

pub(crate) fn expr(p: &mut Parser) -> Option<CompletedMarker> {
    expr_bp(p, 1)
}

/// Contextual keywords that can follow a number without being its unit (`0..4 default 2`).
const NOT_UNITS: &[&str] = &[
    "implies", "default", "required", "unless", "by", "per", "except", "live", "via", "through", "parent",
    "unique", "converges",
];

const BP_TERNARY: u8 = 2;
const BP_NOT: u8 = 8;
const BP_CMP: u8 = 8;
const BP_UNARY: u8 = 14;

/// (left binding power, right binding power) of an infix operator at the cursor.
fn infix_bp(p: &Parser) -> Option<(u8, u8)> {
    Some(match p.current() {
        IF_KW => (BP_TERNARY, BP_TERNARY),
        IDENT if p.nth_text(0) == "implies" => (4, 5),
        OR_KW => (5, 6),
        AND_KW => (6, 7),
        EQ2 | NEQ | LT | GT | LE | GE | IN_KW | IS_KW => (BP_CMP, 9),
        NOT_KW if p.nth(1) == IN_KW => (BP_CMP, 9),
        // right-associative, between comparisons and ranges: `a ?? b == c` is `(a ?? b) == c`
        QUESTION2 => (10, 10),
        DOT2 => (11, 12),
        PLUS | MINUS => (12, 13),
        STAR | SLASH => (13, 14),
        _ => return None,
    })
}

fn expr_bp(p: &mut Parser, min_bp: u8) -> Option<CompletedMarker> {
    if !p.enter() {
        return None;
    }
    let r = expr_bp_inner(p, min_bp);
    p.leave();
    r
}

fn expr_bp_inner(p: &mut Parser, min_bp: u8) -> Option<CompletedMarker> {
    let mut lhs = lhs(p)?;
    while let Some((l, r)) = infix_bp(p) {
        if l < min_bp {
            break;
        }
        if lhs.too_tall() {
            p.too_deep();
            break;
        }
        let m = lhs.precede(p);
        if p.at(IF_KW) {
            // a if cond else b
            p.bump();
            expr_bp(p, BP_TERNARY + 1);
            p.expect(ELSE_KW);
            expr_bp(p, BP_TERNARY);
            lhs = m.complete(p, IF_EXPR);
            continue;
        }
        if p.at(IS_KW) {
            p.bump();
            p.eat(NOT_KW);
        } else if p.at(NOT_KW) {
            p.bump();
            p.bump(); // in
        } else if p.at(IDENT) {
            p.bump_ctx(); // implies
        } else {
            p.bump();
        }
        expr_bp(p, r);
        lhs = m.complete(p, BIN_EXPR);
    }
    Some(lhs)
}

fn lhs(p: &mut Parser) -> Option<CompletedMarker> {
    match p.current() {
        AS_KW => {
            let m = p.start();
            p.bump();
            name_ref(p);
            p.expect(COLON);
            expr_bp(p, 1);
            Some(m.complete(p, AS_EXPR))
        }
        IDENT if p.nth_text(0) == "fail" && !matches!(p.nth(1), DOT | EQ | COMMA | R_PAREN | NEWLINE) => {
            let m = p.start();
            p.bump_ctx();
            expr_bp(p, BP_TERNARY);
            Some(m.complete(p, FAIL_EXPR))
        }
        NOT_KW => {
            let m = p.start();
            p.bump();
            expr_bp(p, BP_NOT);
            Some(m.complete(p, PREFIX_EXPR))
        }
        MINUS => {
            let m = p.start();
            p.bump();
            expr_bp(p, BP_UNARY);
            Some(m.complete(p, PREFIX_EXPR))
        }
        IDENT if p.nth(1) == FAT_ARROW && !p.no_bare_lambda => Some(lambda(p)),
        L_PAREN if lambda_params_ahead(p) => Some(lambda(p)),
        _ => {
            let a = atom(p)?;
            Some(postfix(p, a))
        }
    }
}

/// `(a, b) =>` ahead?
fn lambda_params_ahead(p: &Parser) -> bool {
    let mut k = 1;
    loop {
        match p.nth(k) {
            IDENT | COMMA => k += 1,
            R_PAREN => return p.nth(k + 1) == FAT_ARROW,
            _ => return false,
        }
    }
}

/// `x => body` or `(a, b) => body`
fn lambda(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    let params = p.start();
    if p.eat(L_PAREN) {
        while p.at(IDENT) {
            let pm = p.start();
            name(p);
            pm.complete(p, PARAM);
            if !p.eat(COMMA) {
                break;
            }
        }
        p.expect(R_PAREN);
    } else {
        let pm = p.start();
        name(p);
        pm.complete(p, PARAM);
    }
    params.complete(p, PARAM_LIST);
    p.expect(FAT_ARROW);
    expr_bp(p, 1);
    m.complete(p, LAMBDA_EXPR)
}

fn postfix(p: &mut Parser, mut lhs: CompletedMarker) -> CompletedMarker {
    loop {
        if lhs.too_tall() && matches!(p.current(), DOT | QUESTION_DOT | L_PAREN | L_BRACK) {
            p.too_deep();
            return lhs;
        }
        match p.current() {
            DOT | QUESTION_DOT => {
                let m = lhs.precede(p);
                p.bump();
                if p.at(IDENT) || p.current().is_keyword() {
                    let n = p.start();
                    p.bump_as(IDENT);
                    n.complete(p, NAME_REF);
                } else {
                    p.error("expected a field or method name");
                }
                lhs = m.complete(p, FIELD_EXPR);
            }
            L_PAREN => {
                let m = lhs.precede(p);
                arg_list(p);
                lhs = m.complete(p, CALL_EXPR);
            }
            L_BRACK => {
                let m = lhs.precede(p);
                p.bump();
                expr(p);
                p.expect(R_BRACK);
                lhs = m.complete(p, INDEX_EXPR);
            }
            _ => return lhs,
        }
    }
}

fn atom(p: &mut Parser) -> Option<CompletedMarker> {
    let saved = p.no_bare_lambda;
    if matches!(p.current(), L_PAREN | L_BRACK | L_BRACE) {
        p.no_bare_lambda = false;
    }
    let result = atom_inner(p);
    p.no_bare_lambda = saved;
    result
}

fn atom_inner(p: &mut Parser) -> Option<CompletedMarker> {
    let m = p.start();
    let kind = match p.current() {
        NUMBER => {
            p.bump();
            if p.at(IDENT) && !NOT_UNITS.contains(&p.nth_text(0)) {
                unit(p);
                QUANTITY
            } else {
                LITERAL
            }
        }
        PERCENT | STRING | FSTRING | DATE | DATETIME | YEAR_MONTH | TRUE_KW | FALSE_KW | NONE_KW => {
            p.bump();
            LITERAL
        }
        IDENT | SELF_KW => {
            p.bump_as(IDENT);
            NAME_REF
        }
        // `@name`: a parameter of an app (the name right after the `@`)
        AT => {
            p.bump();
            if p.at(IDENT) {
                p.bump();
            } else {
                p.error("expected a name after `@` (`@region`)");
            }
            PARAM_REF
        }
        WHEN_KW if p.nth(1) == COLON => {
            p.bump();
            p.bump();
            when_arms(p);
            WHEN_EXPR
        }
        MATCH_KW => {
            p.bump();
            expr(p);
            p.expect(COLON);
            arms(p);
            MATCH_EXPR
        }
        L_PAREN => paren(p),
        L_BRACK => {
            p.bump();
            while !p.at(R_BRACK) && !p.at_eof() {
                element(p);
                if !p.eat(COMMA) {
                    break;
                }
            }
            p.expect(R_BRACK);
            LIST_EXPR
        }
        L_BRACE => {
            p.bump();
            while !p.at(R_BRACE) && !p.at_eof() {
                expr(p);
                if !p.eat(COMMA) {
                    break;
                }
            }
            p.expect(R_BRACE);
            SET_EXPR
        }
        _ => {
            m.abandon(p);
            p.error("expected an expression");
            if !matches!(p.current(), NEWLINE | INDENT | DEDENT | R_PAREN | R_BRACK | R_BRACE | COLON | COMMA)
                && !p.at_eof()
            {
                p.err_and_bump("unexpected token");
            }
            return None;
        }
    };
    Some(m.complete(p, kind))
}

/// `EUR`, `pcs`, or a composite unit written without spaces: `EUR/pcs`.
fn unit(p: &mut Parser) {
    let m = p.start();
    let first = p.nth_range(0);
    p.bump();
    if p.at(SLASH) && p.nth(1) == IDENT {
        let slash = p.nth_range(0);
        let next = p.nth_range(1);
        if let (Some(a), Some(s), Some(b)) = (first, slash, next) {
            if a.end() == s.start() && s.end() == b.start() {
                p.bump();
                p.bump();
            }
        }
    }
    m.complete(p, UNIT);
}

fn paren(p: &mut Parser) -> crate::SyntaxKind {
    p.bump();
    if p.eat(R_PAREN) {
        return TUPLE_EXPR;
    }
    if p.at(IDENT) && p.nth(1) == COLON {
        // named tuple: (due: d, amount: a)
        loop {
            let it = p.start();
            name(p);
            p.expect(COLON);
            expr(p);
            it.complete(p, NAMED_ITEM);
            if !p.eat(COMMA) || p.at(R_PAREN) {
                break;
            }
        }
        p.expect(R_PAREN);
        return TUPLE_EXPR;
    }
    expr(p);
    if p.at(COMMA) {
        while p.eat(COMMA) {
            if p.at(R_PAREN) {
                break;
            }
            expr(p);
        }
        p.expect(R_PAREN);
        return TUPLE_EXPR;
    }
    p.expect(R_PAREN);
    PAREN_EXPR
}

/// List element: an expression or a spread `*list`.
fn element(p: &mut Parser) {
    if p.at(STAR) {
        let m = p.start();
        p.bump();
        expr(p);
        m.complete(p, ARG);
    } else {
        expr(p);
    }
}

fn arg_list(p: &mut Parser) {
    let saved = std::mem::replace(&mut p.no_bare_lambda, false);
    arg_list_inner(p);
    p.no_bare_lambda = saved;
}

fn arg_list_inner(p: &mut Parser) {
    let m = p.start();
    p.bump(); // (
    while !p.at(R_PAREN) && !p.at_eof() {
        let a = p.start();
        if p.at(STAR) {
            p.bump();
            expr(p);
        } else if (p.at(IDENT) || p.current().is_keyword()) && p.nth(1) == EQ {
            let n = p.start();
            p.bump_as(IDENT);
            n.complete(p, NAME);
            p.bump(); // =
            expr(p);
        } else if expr(p).is_none() {
            a.abandon(p);
            break;
        }
        a.complete(p, ARG);
        if !p.eat(COMMA) {
            break;
        }
    }
    p.expect(R_PAREN);
    m.complete(p, ARG_LIST);
}

fn indented_arms(p: &mut Parser, mut arm: impl FnMut(&mut Parser)) {
    if !p.eat(NEWLINE) || !p.eat(INDENT) {
        p.error("expected one arm per line in an indented block");
        return;
    }
    while !p.at(DEDENT) && !p.at_eof() {
        if p.eat(NEWLINE) {
            continue;
        }
        let before = p.position();
        arm(p);
        super::ensure_progress(p, before);
    }
    p.eat(DEDENT);
}

fn when_arms(p: &mut Parser) {
    indented_arms(p, |p| {
        let m = p.start();
        if p.at(IDENT) && p.nth(1) == FAT_ARROW {
            // a bare condition (`ready => X`), not a lambda
            let n = p.start();
            p.bump();
            n.complete(p, NAME_REF);
        } else if !p.eat(ELSE_KW) {
            p.no_bare_lambda = true;
            expr(p);
            p.no_bare_lambda = false;
        }
        p.expect(FAT_ARROW);
        expr(p);
        end_line(p);
        m.complete(p, WHEN_ARM);
    });
}

/// Match arms: `pattern | pattern => expr` or `pattern =>` followed by a block.
pub(crate) fn arms(p: &mut Parser) {
    indented_arms(p, |p| {
        let m = p.start();
        pattern(p);
        while p.eat(PIPE) {
            pattern(p);
        }
        p.expect(FAT_ARROW);
        if p.at(NEWLINE) && p.nth(1) == INDENT {
            stmts::block(p);
        } else {
            expr(p);
            end_line(p);
        }
        m.complete(p, MATCH_ARM);
    });
}

fn pattern(p: &mut Parser) {
    if !p.enter() {
        return;
    }
    pattern_inner(p);
    p.leave();
}

fn pattern_inner(p: &mut Parser) {
    let m = p.start();
    match p.current() {
        IDENT => {
            let n = p.start();
            p.bump();
            n.complete(p, NAME_REF);
            if p.eat(L_PAREN) {
                while !p.at(R_PAREN) && !p.at_eof() {
                    pattern(p);
                    if !p.eat(COMMA) {
                        break;
                    }
                }
                p.expect(R_PAREN);
            }
        }
        STRING | NUMBER | PERCENT | NONE_KW | TRUE_KW | FALSE_KW => p.bump(),
        _ => p.error("expected a pattern"),
    }
    m.complete(p, PATTERN);
}
