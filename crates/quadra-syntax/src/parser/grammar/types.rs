//! Types: paths with generic arguments, optionals, lists, sets, unions, tuples, variants.

use super::super::Parser;
use super::name;
use crate::syntax_kind::SyntaxKind::*;

pub(crate) fn type_(p: &mut Parser) {
    let first = postfix_type(p);
    if p.at(PIPE) {
        let m = first.precede(p);
        while p.eat(PIPE) {
            postfix_type(p);
        }
        m.complete(p, TYPE_UNION);
    }
}

fn postfix_type(p: &mut Parser) -> super::super::CompletedMarker {
    let mut t = primary_type(p);
    while p.at(QUESTION) {
        let m = t.precede(p);
        p.bump();
        t = m.complete(p, TYPE_OPTIONAL);
    }
    t
}

fn primary_type(p: &mut Parser) -> super::super::CompletedMarker {
    let m = p.start();
    match p.current() {
        L_BRACK => {
            p.bump();
            type_(p);
            p.expect(R_BRACK);
            m.complete(p, TYPE_LIST)
        }
        ENUM_KW if p.nth(1) == L_PAREN => {
            // inline enum: enum(Draft, Posted, Cancelled)
            p.bump();
            p.bump();
            loop {
                super::variant(p);
                if !p.eat(COMMA) || p.at(R_PAREN) {
                    break;
                }
            }
            p.expect(R_PAREN);
            m.complete(p, TYPE_ENUM)
        }
        L_BRACE => {
            p.bump();
            type_(p);
            p.expect(R_BRACE);
            m.complete(p, TYPE_SET)
        }
        L_PAREN if p.nth(1) == IDENT && p.nth(2) == COLON => {
            p.bump();
            loop {
                tuple_field(p);
                if !p.eat(COMMA) || p.at(R_PAREN) {
                    break;
                }
            }
            p.expect(R_PAREN);
            m.complete(p, TYPE_TUPLE)
        }
        L_PAREN => {
            p.bump();
            type_(p);
            p.expect(R_PAREN);
            m.complete(p, TYPE_PAREN)
        }
        IDENT | SELF_KW => {
            super::path(p);
            if p.at(LT) {
                let g = p.start();
                p.bump();
                loop {
                    type_(p);
                    if !p.eat(COMMA) {
                        break;
                    }
                }
                p.expect(GT);
                g.complete(p, GENERIC_ARGS);
            }
            // variant payload: `Fixed(amount: Money)`
            if p.at(L_PAREN) && p.nth(1) == IDENT && p.nth(2) == COLON {
                field_list(p);
            }
            m.complete(p, TYPE_PATH)
        }
        _ => {
            p.error("expected a type");
            if !p.at(NEWLINE) && !p.at_eof() {
                p.bump();
            }
            m.complete(p, ERROR)
        }
    }
}

fn tuple_field(p: &mut Parser) {
    let m = p.start();
    name(p);
    p.expect(COLON);
    type_(p);
    m.complete(p, TUPLE_FIELD);
}

/// `(name: Type, …)`: variant payloads and named tuples.
pub(crate) fn field_list(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);
    while !p.at(R_PAREN) && !p.at_eof() && !p.at(NEWLINE) {
        tuple_field(p);
        if !p.eat(COMMA) {
            break;
        }
    }
    p.expect(R_PAREN);
    m.complete(p, FIELD_LIST);
}
