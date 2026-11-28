//! Grammar: top-level items and entity members. See the syntax reference (quadra.run/reference/syntax).

mod exprs;
mod stmts;
mod types;

pub(crate) use exprs::expr;
pub(crate) use stmts::block;
pub(crate) use types::type_;

use super::{CompletedMarker, Parser};
use crate::syntax_kind::SyntaxKind::{self, *};

pub(crate) fn source_file(p: &mut Parser) {
    let m = p.start();
    while !p.at_eof() {
        match p.current() {
            NEWLINE => p.bump(),
            INDENT => {
                let e = p.start();
                p.error("unexpected indentation");
                skip_block(p);
                e.complete(p, ERROR);
            }
            DEDENT => p.bump(),
            _ => item_or_recover(p),
        }
    }
    m.complete(p, SOURCE_FILE);
}

/// Does the current position start a declaration? (Used by statement blocks too, so that
/// `fn` and `extend` can appear inside tests and reactions.)
pub(crate) fn at_item(p: &Parser) -> bool {
    match p.current() {
        FROM_KW | ENTITY_KW | RECORD_KW | TRAIT_KW | EXTEND_KW | ENUM_KW | LEDGER_KW | FN_KW
        | CONST_KW => true,
        IDENT => {
            let t = p.nth_text(0);
            let n1 = p.nth(1);
            match t {
                "temporary" => n1 == ENTITY_KW,
                "builtin" => n1 == FN_KW,
                "patch" | "view" | "report" | "role" | "metric" | "control" | "migration"
                | "change" | "fixture" => n1 == IDENT,
                "test" => n1 == STRING,
                "event" => n1 == IDENT && p.nth(2) == L_PAREN,
                "outbound" => n1 == IDENT && p.nth(2) == IDENT,
                "inbound" => n1 == IDENT && p.nth(2) == IDENT,
                "endpoint" => n1 == IDENT && p.nth(2) == STRING,
                "publish" => n1 == IDENT && p.nth_at_ctx(2, "to"),
                "document" | "label" => n1 == IDENT && p.nth(2) == L_PAREN,
                "attach" => n1 == IDENT && p.nth_at_ctx(2, "to"),
                "seed" => n1 == COLON,
                "on" | "before" => n1 == IDENT && matches!(p.nth(2), DOT | IDENT),
                "deny" | "allow" => n1 == IDENT,
                _ => false,
            }
        }
        _ => false,
    }
}

fn item_or_recover(p: &mut Parser) {
    if at_item(p) {
        item(p);
    } else {
        let m = p.start();
        p.error("expected a declaration (entity, enum, fn, test…)");
        recover_line(p);
        m.complete(p, ERROR);
    }
}

pub(crate) fn item(p: &mut Parser) {
    match p.current() {
        FROM_KW => import(p),
        ENTITY_KW | RECORD_KW | TRAIT_KW | EXTEND_KW => entity(p),
        ENUM_KW => enum_(p),
        LEDGER_KW => ledger(p),
        FN_KW => fn_(p),
        CONST_KW => const_(p),
        IDENT => match p.nth_text(0) {
            "temporary" | "patch" => entity(p),
            "builtin" => fn_(p),
            "seed" => keyword_block(p, SEED),
            "on" | "before" => reaction(p),
            "view" => view(p),
            "report" => report(p),
            "role" => role(p),
            "deny" | "allow" => rule(p),
            "metric" => metric(p),
            "control" => named_member_block(p, CONTROL),
            "migration" => named_block(p, MIGRATION),
            "change" => named_block(p, CHANGE),
            "fixture" => fixture(p),
            "test" => test(p),
            "event" => event(p),
            "outbound" => outbound(p),
            "inbound" => inbound(p),
            "endpoint" => endpoint(p),
            "publish" => publish(p),
            "document" | "label" => document(p),
            "attach" => attach(p),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    }
}

// ---- helpers ----------------------------------------------------------------------------

/// Skip to the end of the logical line, including a following indented block.
pub(crate) fn recover_line(p: &mut Parser) {
    while !p.at_eof() && !p.at(NEWLINE) && !p.at(DEDENT) {
        if p.at(INDENT) {
            skip_block(p);
            return;
        }
        p.bump();
    }
    p.eat(NEWLINE);
    if p.at(INDENT) {
        skip_block(p);
    }
}

/// Skip a balanced INDENT … DEDENT region.
pub(crate) fn skip_block(p: &mut Parser) {
    if !p.at(INDENT) {
        return;
    }
    let mut depth = 0usize;
    while !p.at_eof() {
        match p.current() {
            INDENT => depth += 1,
            DEDENT => {
                depth -= 1;
                if depth == 0 {
                    p.bump();
                    return;
                }
            }
            _ => {}
        }
        p.bump();
    }
}

/// End of a logical line: either a NEWLINE or the end of a block just parsed.
pub(crate) fn end_line(p: &mut Parser) {
    if p.eat(NEWLINE) || p.prev_was(DEDENT) || p.at_eof() {
        return;
    }
    let m = p.start();
    p.error("expected end of line");
    while !p.at_eof() && !p.at(NEWLINE) && !p.at(DEDENT) && !p.at(INDENT) {
        p.bump();
    }
    p.eat(NEWLINE);
    if p.at(INDENT) {
        skip_block(p);
    }
    m.complete(p, ERROR);
}

pub(crate) fn name(p: &mut Parser) {
    if p.at(IDENT) {
        let m = p.start();
        p.bump();
        m.complete(p, NAME);
    } else {
        p.error("expected a name");
    }
}

pub(crate) fn name_ref(p: &mut Parser) {
    if p.at(IDENT) || p.at(SELF_KW) {
        let m = p.start();
        p.bump_as(IDENT);
        m.complete(p, NAME_REF);
    } else {
        p.error("expected a name");
    }
}

/// `a.b.c` as a PATH of NAME_REFs.
pub(crate) fn path(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    name_ref(p);
    while p.at(DOT) && p.nth(1) == IDENT {
        p.bump();
        name_ref(p);
    }
    m.complete(p, PATH)
}

/// `(a, b, c)` or `a, b` as a NAME_LIST of NAME_REFs.
fn name_list(p: &mut Parser, parens: bool) {
    let m = p.start();
    if parens {
        p.expect(L_PAREN);
    }
    name_ref(p);
    while p.eat(COMMA) {
        name_ref(p);
    }
    if parens {
        p.expect(R_PAREN);
    }
    m.complete(p, NAME_LIST);
}

pub(crate) fn param_list(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);
    while !p.at(R_PAREN) && !p.at_eof() && !p.at(NEWLINE) {
        let pm = p.start();
        name(p);
        if p.eat(COLON) {
            type_(p);
        }
        if p.eat(EQ) {
            expr(p);
        }
        pm.complete(p, PARAM);
        if !p.eat(COMMA) {
            break;
        }
    }
    p.expect(R_PAREN);
    m.complete(p, PARAM_LIST);
}

pub(crate) fn ret_type(p: &mut Parser) {
    if p.at(ARROW) {
        let m = p.start();
        p.bump();
        type_(p);
        m.complete(p, RET_TYPE);
    }
}

// ---- items ------------------------------------------------------------------------------

fn import(p: &mut Parser) {
    let m = p.start();
    p.bump(); // from
    path(p);
    p.expect(IMPORT_KW);
    name_ref(p);
    while p.eat(COMMA) {
        name_ref(p);
    }
    end_line(p);
    m.complete(p, IMPORT);
}

fn entity(p: &mut Parser) {
    let m = p.start();
    if p.at_ctx("temporary") {
        p.bump_ctx();
    }
    if p.at_ctx("patch") {
        p.bump_ctx();
        path(p);
    } else if p.eat(EXTEND_KW) {
        path(p);
    } else {
        if !(p.eat(ENTITY_KW) || p.eat(RECORD_KW) || p.eat(TRAIT_KW)) {
            p.error("expected `entity`, `record` or `trait`");
        }
        name(p);
        if p.at(IS_KW) {
            let c = p.start();
            p.bump();
            path(p);
            while p.eat(COMMA) {
                path(p);
            }
            c.complete(p, IS_CLAUSE);
        }
        // `record ShopOrder from json:`, `record PosRow from csv(sep=";", header=false):`
        if p.at(FROM_KW) {
            let c = p.start();
            p.bump();
            name(p);
            if p.eat(L_PAREN) {
                while !p.at(R_PAREN) && !p.at_eof() && !p.at(NEWLINE) {
                    let o = p.start();
                    name(p);
                    p.expect(EQ);
                    expr(p);
                    o.complete(p, FORMAT_OPTION);
                    if !p.eat(COMMA) {
                        break;
                    }
                }
                p.expect(R_PAREN);
            }
            c.complete(p, FORMAT_CLAUSE);
        }
    }
    p.expect(COLON);
    member_block(p, MemberCtx::Entity);
    m.complete(p, ENTITY);
}

fn enum_(p: &mut Parser) {
    let m = p.start();
    p.bump();
    name(p);
    p.expect(COLON);
    let block = p.eat(NEWLINE) && p.eat(INDENT);
    loop {
        let l = p.start();
        variant(p);
        while p.eat(COMMA) {
            if p.at(NEWLINE) || p.at(DEDENT) {
                break;
            }
            variant(p);
        }
        l.complete(p, VARIANT_LIST);
        if !block {
            end_line(p);
            break;
        }
        p.eat(NEWLINE);
        if p.at(DEDENT) || p.at_eof() {
            p.eat(DEDENT);
            break;
        }
    }
    m.complete(p, ENUM);
}

pub(crate) fn variant(p: &mut Parser) {
    let m = p.start();
    name(p);
    if p.at(L_PAREN) {
        types::field_list(p);
    }
    m.complete(p, VARIANT);
}

fn ledger(p: &mut Parser) {
    let m = p.start();
    p.bump();
    name(p);
    p.expect(COLON);
    member_block(p, MemberCtx::Ledger);
    m.complete(p, LEDGER);
}

pub(crate) fn fn_(p: &mut Parser) {
    let m = p.start();
    if p.at_ctx("builtin") {
        p.bump_ctx();
    }
    p.expect(FN_KW);
    name(p);
    param_list(p);
    ret_type(p);
    if p.eat(COLON) {
        block(p);
    } else {
        end_line(p);
    }
    m.complete(p, FN);
}

fn const_(p: &mut Parser) {
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
    m.complete(p, CONST);
}

/// `seed:` followed by a block of statements.
fn keyword_block(p: &mut Parser, kind: SyntaxKind) {
    let m = p.start();
    p.bump_ctx();
    p.expect(COLON);
    block(p);
    m.complete(p, kind);
}

/// `migration name:` / `change name:` followed by statements.
fn named_block(p: &mut Parser, kind: SyntaxKind) {
    let m = p.start();
    p.bump_ctx();
    name(p);
    p.expect(COLON);
    block(p);
    m.complete(p, kind);
}

fn named_member_block(p: &mut Parser, kind: SyntaxKind) {
    let m = p.start();
    p.bump_ctx();
    name(p);
    p.expect(COLON);
    member_block(p, MemberCtx::Meta);
    m.complete(p, kind);
}

fn metric(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name(p);
    param_list(p);
    ret_type(p);
    p.expect(COLON);
    member_block(p, MemberCtx::Meta);
    m.complete(p, METRIC);
}

fn reaction(p: &mut Parser) {
    let m = p.start();
    let before = p.at_ctx("before");
    p.bump_ctx();
    path(p);
    if !before {
        match p.nth_text(0) {
            "enters" => {
                p.bump_ctx();
                name_ref(p);
            }
            "becomes" => {
                p.bump_ctx();
                expr(p);
            }
            "created" | "changes" | "deleted" => p.bump_ctx(),
            // the outcome of an event's delivery: `on OrderShipped delivered as d:`
            "delivered" | "failed" | "expired" | "skipped" | "rejected" => p.bump_ctx(),
            _ => p.error(
                "expected `enters`, `becomes`, `created` or `changes` (or, for an event, `delivered`, `failed`, `expired` or `skipped`; for an inbound, `rejected`)",
            ),
        }
    }
    p.expect(AS_KW);
    name(p);
    if p.at_ctx("converges") {
        p.bump_ctx();
    }
    p.expect(COLON);
    block(p);
    m.complete(p, REACTION);
}

/// `event SendEmail(to: Email, template: Text)`
fn event(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name(p);
    types::field_list(p);
    end_line(p);
    m.complete(p, EVENT);
}

/// `inbound ShopOrder via shop path "/orders" as m: ShopOrderDoc:`, then lines of policy (`key
/// m.id`) and the handler: statements, as in an action
fn inbound(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name(p);
    if p.at_ctx("via") {
        p.bump_ctx();
        name(p);
    } else {
        p.error("expected `via` and the connection of quadra.toml: `inbound ShopOrder via shop`");
    }
    if p.at_ctx("path") {
        p.bump_ctx();
        p.expect(STRING);
    }
    p.expect(AS_KW);
    name(p);
    p.expect(COLON);
    path(p);
    p.expect(COLON);
    if p.at(NEWLINE) && p.nth(1) == INDENT {
        p.bump();
        p.bump();
        while p.eat(NEWLINE) {}
        loop {
            let c = p.start();
            if p.at_ctx("key") && !matches!(p.nth(1), EQ | L_PAREN | DOT | COLON_EQ) {
                p.bump_ctx();
                expr(p);
            } else if p.at_ctx("pull") && p.nth(1) == STRING {
                // `pull "/orders?since={cursor}" every 5 minutes`
                p.bump_ctx();
                p.bump();
                if p.at_ctx("every") {
                    p.bump_ctx();
                    expr(p);
                } else {
                    p.error("expected `every` and how often: `pull \"/orders\" every 5 minutes`");
                }
            } else if p.at_ctx("items") && p.nth(1) == IDENT {
                // `items ShopOrders.orders`: the answer is a ShopOrders, its `orders` the messages
                p.bump_ctx();
                path(p);
            } else if p.at_ctx("cursor") && !matches!(p.nth(1), EQ | L_PAREN | DOT | COLON_EQ) {
                // `cursor m.updated_at`: where the next pull starts
                p.bump_ctx();
                expr(p);
            } else if p.at_ctx("retry") && p.nth(1) == FOR_KW {
                // `retry for 1 hour`
                p.bump_ctx();
                p.bump();
                expr(p);
            } else if p.at_ctx("backoff") && !matches!(p.nth(1), EQ | L_PAREN | DOT | COLON_EQ) {
                p.bump_ctx();
                expr(p);
            } else if p.at_ctx("ordered") && p.nth_at_ctx(1, "by") {
                p.bump_ctx();
                p.bump_ctx();
                expr(p);
            } else if p.at_ctx("atomic") && p.nth_at_ctx(1, "per") {
                // `atomic per file` (all the rows of a file or none), `atomic per row`
                p.bump_ctx();
                p.bump_ctx();
                if p.at_ctx("file") || p.at_ctx("row") {
                    p.bump_ctx();
                } else {
                    p.error("expected `file` or `row`: `atomic per file`, `atomic per row`");
                }
            } else if p.at_ctx("latest") && p.nth_at_ctx(1, "by") {
                // `latest by m.product using m.updated_at`
                p.bump_ctx();
                p.bump_ctx();
                expr(p);
                if p.at_ctx("using") {
                    p.bump_ctx();
                    expr(p);
                } else {
                    p.error("expected `using` and what says which is newer: `latest by m.product using m.updated_at`");
                }
            } else {
                c.abandon(p);
                break;
            }
            end_line(p);
            c.complete(p, OUTBOUND_CLAUSE);
            while p.eat(NEWLINE) {}
        }
        let blk = p.start();
        while !p.at(DEDENT) && !p.at_eof() {
            if p.eat(NEWLINE) {
                continue;
            }
            let before = p.position();
            stmts::stmt(p);
            ensure_progress(p, before);
        }
        blk.complete(p, BLOCK);
        p.eat(DEDENT);
    } else {
        p.error("expected an indented block: what to do with the message");
    }
    m.complete(p, INBOUND);
}

/// `endpoint GET "/price" via pos as q: PriceQuery -> PosPrice:`: a policy line (`key …`), then
/// the code whose last expression is the answer
fn endpoint(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name(p);
    p.expect(STRING);
    if p.at_ctx("via") {
        p.bump_ctx();
        name(p);
    } else {
        p.error(
            "expected `via` and the connection of quadra.toml: `endpoint GET \"/price\" via pos`",
        );
    }
    p.expect(AS_KW);
    name(p);
    p.expect(COLON);
    path(p);
    p.expect(ARROW);
    path(p);
    p.expect(COLON);
    if p.at(NEWLINE) && p.nth(1) == INDENT {
        p.bump();
        p.bump();
        while p.eat(NEWLINE) {}
        loop {
            let c = p.start();
            if p.at_ctx("key") && !matches!(p.nth(1), EQ | L_PAREN | DOT | COLON_EQ) {
                p.bump_ctx();
                expr(p);
            } else {
                c.abandon(p);
                break;
            }
            end_line(p);
            c.complete(p, OUTBOUND_CLAUSE);
            while p.eat(NEWLINE) {}
        }
        let b = p.start();
        let blk = p.start();
        while !p.at(DEDENT) && !p.at_eof() {
            if p.eat(NEWLINE) {
                continue;
            }
            let before = p.position();
            stmts::stmt(p);
            ensure_progress(p, before);
        }
        blk.complete(p, BLOCK);
        b.complete(p, BLOCK_EXPR);
        p.eat(DEDENT);
    } else {
        p.error("expected an indented block: the answer");
    }
    m.complete(p, ENDPOINT);
}

/// A line of policy of an `outbound … as e:` (or a `publish`): `reply ErpAck`, `retry for 3
/// days`, `retry 10 times`, `ordered by …`, `latest by …`, `expires after …`, `backoff …`,
/// `unless …`, `delay …`, `window …`, `key …`. False (nothing eaten) when the line is code.
fn policy_line(p: &mut Parser) -> bool {
    if p.at_ctx("reply") && p.nth(1) == IDENT {
        p.bump_ctx();
        path(p);
    } else if p.at_ctx("retry") && p.nth(1) == FOR_KW {
        // `retry for 3 days`
        p.bump_ctx();
        p.bump();
        expr(p);
    } else if p.at_ctx("retry") && !p.nth_at_ctx(1, "times") && p.nth(1) != L_PAREN {
        // `retry 10 times` (the number alone: `10 times` is not a quantity)
        p.bump_ctx();
        let n = p.start();
        p.expect(NUMBER);
        n.complete(p, LITERAL);
        if p.at_ctx("times") {
            p.bump_ctx();
        } else {
            p.error("expected `times`: `retry 10 times` (or `retry for 3 days`)");
        }
    } else if (p.at_ctx("ordered") || p.at_ctx("latest")) && p.nth_at_ctx(1, "by") {
        // `ordered by e.customer`, `latest by e.product`
        p.bump_ctx();
        p.bump_ctx();
        expr(p);
    } else if p.at_ctx("expires") && p.nth_at_ctx(1, "after") {
        // `expires after 1 day`
        p.bump_ctx();
        p.bump_ctx();
        expr(p);
    } else if ["backoff", "unless", "delay", "window", "key"]
        .iter()
        .any(|k| p.at_ctx(k))
        && !matches!(p.nth(1), EQ | L_PAREN | DOT | COLON_EQ)
    {
        // `backoff 30 seconds .. 1 hour`, `unless e.order.cancelled`, `delay 10
        // minutes`, `window "02:00" .. "04:00"`, `key f"ship-{e.order.number}"`
        p.bump_ctx();
        expr(p);
    } else {
        return false;
    }
    true
}

/// `publish Product to shop as p:`, then lines (`when p.sellable`, `on price, stock`,
/// `debounce 30 seconds`) and the code that builds the request from the record
fn publish(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    path(p);
    p.bump_ctx(); // to
    name(p);
    p.expect(AS_KW);
    name(p);
    p.expect(COLON);
    if !(p.at(NEWLINE) && p.nth(1) == INDENT) {
        p.error("expected an indented block: the request built from the record");
        m.complete(p, PUBLISH);
        return;
    }
    p.bump();
    p.bump();
    while p.eat(NEWLINE) {}
    loop {
        let c = p.start();
        if p.at(WHEN_KW) {
            p.bump();
            expr(p);
        } else if p.at_ctx("on") && p.nth(1) == IDENT {
            // `on price, stock`: the fields whose change sends it again
            p.bump_ctx();
            name_ref(p);
            while p.eat(COMMA) {
                name_ref(p);
            }
        } else if p.at_ctx("debounce") && !matches!(p.nth(1), EQ | L_PAREN | DOT | COLON_EQ) {
            p.bump_ctx();
            expr(p);
        } else if !policy_line(p) {
            c.abandon(p);
            break;
        }
        end_line(p);
        c.complete(p, OUTBOUND_CLAUSE);
        while p.eat(NEWLINE) {}
    }
    let b = p.start();
    let blk = p.start();
    while !p.at(DEDENT) && !p.at_eof() {
        if p.eat(NEWLINE) {
            continue;
        }
        let before = p.position();
        stmts::stmt(p);
        ensure_progress(p, before);
    }
    blk.complete(p, BLOCK);
    b.complete(p, BLOCK_EXPR);
    p.eat(DEDENT);
    m.complete(p, PUBLISH);
}

/// `outbound OrderShipped via shop path "/hooks/orders"`, `outbound SendEmail via mail`
fn outbound(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name(p);
    if p.at_ctx("via") {
        p.bump_ctx();
        name(p);
    } else {
        p.error(
            "expected `via` and the connection of quadra.toml: `outbound OrderShipped via shop`",
        );
    }
    if p.at_ctx("path") {
        p.bump_ctx();
        p.expect(STRING);
    }
    // `batch 50 within 30 seconds`: up to 50 events in one request
    if p.at_ctx("batch") && p.nth(1) == NUMBER {
        let c = p.start();
        p.bump_ctx();
        let n = p.start();
        p.bump();
        n.complete(p, LITERAL);
        if p.at_ctx("within") {
            p.bump_ctx();
            expr(p);
        }
        c.complete(p, OUTBOUND_CLAUSE);
    }
    // `as e:` and a block: lines of policy (`reply ErpAck`), then the code that builds the
    // request from the event
    if p.at(AS_KW) {
        p.bump();
        name(p);
        p.expect(COLON);
        if p.at(NEWLINE) && p.nth(1) == INDENT {
            p.bump();
            p.bump();
            while p.eat(NEWLINE) {}
            // the lines of policy, each a keyword and its value
            loop {
                let c = p.start();
                if !policy_line(p) {
                    c.abandon(p);
                    break;
                }
                end_line(p);
                c.complete(p, OUTBOUND_CLAUSE);
                while p.eat(NEWLINE) {}
            }
            let b = p.start();
            let blk = p.start();
            while !p.at(DEDENT) && !p.at_eof() {
                if p.eat(NEWLINE) {
                    continue;
                }
                let before = p.position();
                stmts::stmt(p);
                ensure_progress(p, before);
            }
            blk.complete(p, BLOCK);
            b.complete(p, BLOCK_EXPR);
            p.eat(DEDENT);
        } else {
            p.error("expected an indented block: the request built from the event");
        }
    } else {
        end_line(p);
    }
    m.complete(p, OUTBOUND);
}

// ---- documents and labels (`.qdp`) -----------------------------------------------------

/// The options of the elements: a name and a value (`size 14`), or a flag alone (`bold`).
pub(crate) const PRINT_OPTIONS: &[&str] = &[
    "size",
    "bold",
    "italic",
    "align",
    "color",
    "level",
    "width",
    "height",
    "widths",
    "margin",
    "landscape",
    "x",
    "y",
    "x2",
    "y2",
    "thickness",
    "type",
    "human",
    "dpi",
    "gap",
    "fill",
    "header",
    "font",
    "format",
    "valign",
];
/// The options that take no value.
pub(crate) const PRINT_FLAGS: &[&str] = &["bold", "italic", "landscape", "human"];
/// The options whose value is a word, not a name of the code (`align right`, `type qr`).
const PRINT_WORDS: &[&str] = &["align", "valign", "type"];

/// Is the line at an option? A flag always; a name with a value when something follows it
/// (`text x` is the value `x`, `box x 2` the option `x`).
fn at_print_opt(p: &Parser) -> bool {
    if !p.at(IDENT) || !PRINT_OPTIONS.contains(&p.nth_text(0)) {
        return false;
    }
    PRINT_FLAGS.contains(&p.nth_text(0)) || !matches!(p.nth(1), NEWLINE | COLON | DEDENT | AS_KW)
}

/// The value of an option: a number alone when an option follows it (`size 14 bold`: not the
/// quantity `14 bold`), else an expression.
fn print_value(p: &mut Parser) {
    if p.at(NUMBER) && p.nth(1) == IDENT && PRINT_OPTIONS.contains(&p.nth_text(1)) {
        let m = p.start();
        p.bump();
        m.complete(p, LITERAL);
    } else {
        expr(p);
    }
}

fn print_opts(p: &mut Parser) {
    while at_print_opt(p) {
        let o = p.start();
        let flag = PRINT_FLAGS.contains(&p.nth_text(0));
        let word = PRINT_WORDS.contains(&p.nth_text(0));
        p.bump_ctx();
        if word && p.at(IDENT) {
            name(p);
        } else if !flag {
            print_value(p);
        }
        o.complete(p, PRINT_OPT);
    }
}

/// `document InvoicePdf(inv: Invoice):` and its elements; `label … width 100 height 150:`
fn document(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name(p);
    param_list(p);
    print_opts(p);
    p.expect(COLON);
    print_block(p);
    m.complete(p, DOCUMENT);
}

fn print_block(p: &mut Parser) {
    indented(p, print_item);
}

fn print_item(p: &mut Parser) {
    let m = p.start();
    match p.current() {
        LET_KW => {
            p.bump();
            name(p);
            p.expect(EQ);
            expr(p);
            end_line(p);
            m.complete(p, PRINT_LET);
        }
        IF_KW => {
            p.bump();
            // `if let a = inv.customer.address:`: the value, when there is one, named
            if p.eat(LET_KW) {
                name(p);
                p.expect(EQ);
            }
            expr(p);
            p.expect(COLON);
            print_block(p);
            while p.eat(NEWLINE) {}
            if p.eat(ELSE_KW) {
                p.expect(COLON);
                print_block(p);
            }
            m.complete(p, PRINT_IF);
        }
        FOR_KW => {
            p.bump();
            name(p);
            p.expect(IN_KW);
            expr(p);
            p.expect(COLON);
            print_block(p);
            m.complete(p, PRINT_FOR);
        }
        IDENT => {
            let page = p.at_ctx("page");
            p.bump_ctx();
            // the value, unless the line goes on with an option, a block or nothing
            if page && p.at(IDENT) && !at_print_opt(p) {
                // `page A4`: the size, a word
                name(p);
            } else if !at_print_opt(p)
                && !p.at(COLON)
                && !p.at(NEWLINE)
                && !p.at(AS_KW)
                && !p.at_eof()
                && !p.at(DEDENT)
            {
                print_value(p);
            }
            print_opts(p);
            if p.eat(AS_KW) {
                name(p);
            }
            if p.eat(COLON) {
                if p.at(NEWLINE) && p.nth(1) == INDENT {
                    print_block(p);
                } else {
                    expr(p);
                    end_line(p);
                }
            } else {
                end_line(p);
            }
            m.complete(p, PRINT_ITEM);
        }
        _ => {
            p.error("expected an element of the document: `text`, `table`, `row`, `image`, `if`, `for`, `let`…");
            recover_line(p);
            m.complete(p, ERROR);
        }
    }
}

/// `attach InvoicePdf to Invoice.pdf when state is Posted`
fn attach(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name_ref(p);
    p.bump_ctx(); // `to`
    path(p);
    // `signed "company"`: the document signed (PAdES) by a signer of quadra.toml
    if p.at_ctx("signed") {
        p.bump_ctx();
        p.expect(STRING);
    }
    if p.eat(WHEN_KW) {
        expr(p);
    }
    end_line(p);
    m.complete(p, ATTACH);
}

fn fixture(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name(p);
    if p.at_ctx("extends") {
        p.bump_ctx();
        path(p);
    }
    p.expect(COLON);
    block(p);
    m.complete(p, FIXTURE);
}

fn test(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    p.expect(STRING);
    p.expect(COLON);
    block(p);
    m.complete(p, TEST);
}

fn view(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name(p);
    if p.at(L_PAREN) {
        param_list(p);
    }
    p.expect(COLON);
    indented(p, |p| {
        let it = p.start();
        if p.at(IDENT) {
            p.bump_ctx();
        } else {
            p.error("expected `header`, `table`, `tab`, `footer` or `actions`");
        }
        if !p.at(COLON) {
            expr(p);
        }
        if p.eat(AS_KW) {
            name(p);
        }
        p.expect(COLON);
        expr_list(p);
        end_line(p);
        it.complete(p, VIEW_ITEM);
    });
    m.complete(p, VIEW);
}

fn report(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name(p);
    p.expect(COLON);
    indented(p, |p| {
        let it = p.start();
        if p.eat(FROM_KW) {
            path(p);
            if p.eat(AS_KW) {
                name(p);
            }
            if p.eat(WHERE_KW) {
                expr(p);
            }
            end_line(p);
        } else if p.at_ctx("values") {
            p.bump_ctx();
            p.expect(COLON);
            indented(p, |p| {
                let f = p.start();
                name(p);
                if p.at(EQ) {
                    value(p);
                } else {
                    p.error("expected `=`");
                }
                end_line(p);
                f.complete(p, FIELD);
            });
        } else if p.at(IDENT) {
            p.bump_ctx();
            p.expect(COLON);
            expr_list(p);
            end_line(p);
        } else {
            p.error("expected `from`, `rows`, `columns` or `values`");
            recover_line(p);
        }
        it.complete(p, REPORT_ITEM);
    });
    m.complete(p, REPORT);
}

fn role(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name(p);
    if p.at(IS_KW) {
        let c = p.start();
        p.bump();
        path(p);
        c.complete(p, IS_CLAUSE);
    }
    p.expect(COLON);
    indented(p, rule);
    m.complete(p, ROLE);
}

const VERBS: &[&str] = &[
    "read", "create", "edit", "run", "erase", "all", "hide", "show", "derived", "delete",
];

fn rule(p: &mut Parser) {
    let m = p.start();
    if p.at_ctx("deny") || p.at_ctx("allow") {
        p.bump_ctx();
    }
    let mut verbs = 0;
    while p.at(IDENT) && VERBS.contains(&p.nth_text(0)) && p.nth(1) != DOT {
        p.bump_ctx();
        verbs += 1;
        if !(p.at(COMMA) && p.nth(1) == IDENT && VERBS.contains(&p.nth_text(1))) {
            break;
        }
        p.bump();
    }
    if verbs == 0 {
        p.error("expected a permission (read, create, edit, run, all, hide, show)");
    }
    if p.at_ctx("history") {
        p.bump_ctx();
    }
    loop {
        if p.at(STAR) {
            p.bump();
        } else {
            path(p);
        }
        if !p.eat(COMMA) {
            break;
        }
    }
    if p.eat(WHERE_KW) {
        expr(p);
    }
    end_line(p);
    m.complete(p, RULE);
}

/// Comma-separated expressions until the end of the line.
pub(crate) fn expr_list(p: &mut Parser) {
    if p.at(NEWLINE) {
        return;
    }
    expr(p);
    while p.eat(COMMA) {
        expr(p);
    }
}

/// `NEWLINE INDENT item* DEDENT`, calling `f` for each line.
pub(crate) fn indented(p: &mut Parser, mut f: impl FnMut(&mut Parser)) {
    if !p.eat(NEWLINE) || !p.eat(INDENT) {
        p.error("expected an indented block");
        return;
    }
    while !p.at(DEDENT) && !p.at_eof() {
        if p.eat(NEWLINE) {
            continue;
        }
        let before = p.position();
        f(p);
        ensure_progress(p, before);
    }
    p.eat(DEDENT);
}

/// Error recovery guard: if a grammar rule consumed nothing, skip one token.
pub(crate) fn ensure_progress(p: &mut Parser, before: usize) {
    if p.position() == before && !p.at_eof() {
        p.err_and_bump("unexpected token");
    }
}

// ---- members ----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberCtx {
    Entity,
    Ledger,
    /// metric / control: `label: expr` lines are metadata, not fields
    Meta,
}

fn member_block(p: &mut Parser, ctx: MemberCtx) {
    let m = p.start();
    indented(p, |p| member(p, ctx));
    m.complete(p, MEMBER_LIST);
}

/// Next token after a contextual keyword candidate makes it a declaration, not a keyword.
fn is_decl_after(p: &Parser) -> bool {
    matches!(p.nth(1), COLON | COLON_EQ | EQ) || p.nth_at_ctx(1, "default")
}

fn member(p: &mut Parser, ctx: MemberCtx) {
    if p.at(FN_KW) || p.at_ctx("builtin") {
        fn_(p);
        return;
    }
    if p.at(IDENT) && !is_decl_after(p) {
        match p.nth_text(0) {
            "check" | "warn" => return check(p),
            "unique" if p.nth(1) == L_PAREN => return unique(p),
            "frozen" if p.nth(1) == WHEN_KW => return frozen(p),
            "live" if p.nth(1) == IDENT => return live(p),
            "flow" => return flow(p),
            "action" => return action(p),
            "post" if p.nth_at_ctx(1, "to") => return post(p),
            "load" if p.nth(1) == FROM_KW => return load(p),
            "expire" | "retain" if p.nth(1) == NUMBER => {
                let m = p.start();
                retention(p);
                end_line(p);
                m.complete(p, RETENTION);
                return;
            }
            "history" if p.nth(1) == NUMBER => {
                let m = p.start();
                p.bump_ctx();
                p.bump();
                p.expect(IDENT);
                end_line(p);
                m.complete(p, HISTORY);
                return;
            }
            "history" if matches!(p.nth_text(1), "full" | "none" | "last") => {
                let m = p.start();
                p.bump_ctx();
                let last = p.at_ctx("last");
                p.bump_ctx();
                if last {
                    p.expect(NUMBER);
                }
                end_line(p);
                m.complete(p, HISTORY);
                return;
            }
            "deletable" if matches!(p.nth(1), NEWLINE | COMMA | DEDENT) => {
                let m = p.start();
                p.bump_ctx();
                if p.eat(COMMA) {
                    if p.at_ctx("trash") {
                        p.bump_ctx();
                        p.expect(NUMBER);
                        p.expect(IDENT);
                    } else {
                        p.error("expected `trash` and how long its records stay there (`trash 30 days`)");
                    }
                }
                end_line(p);
                m.complete(p, DELETABLE);
                return;
            }
            // `ns cbc "urn:…"`, `ns "urn:…"`: the namespaces of a record `from xml`
            "ns" if p.nth(1) == STRING || (p.nth(1) == IDENT && p.nth(2) == STRING) => {
                let m = p.start();
                p.bump_ctx();
                if p.at(IDENT) {
                    name(p);
                }
                p.bump();
                end_line(p);
                m.complete(p, NAMESPACE);
                return;
            }
            "key" | "measure" | "attr" if p.nth(1) == IDENT => {
                let m = p.start();
                p.bump_ctx();
                field_rest(p, ctx);
                m.complete(p, FIELD);
                return;
            }
            _ => {}
        }
    }
    if p.at(IDENT) && is_decl_after(p) {
        if ctx == MemberCtx::Meta && p.nth(1) == COLON {
            let m = p.start();
            name(p);
            p.bump();
            meta_value(p);
            end_line(p);
            m.complete(p, META);
            return;
        }
        let m = p.start();
        field_rest(p, ctx);
        m.complete(p, FIELD);
        return;
    }
    let m = p.start();
    p.error("expected a field, formula, check, flow or action");
    recover_line(p);
    m.complete(p, ERROR);
}

/// Value of a metadata line: an expression, or `role Name`.
pub(crate) fn meta_value(p: &mut Parser) {
    if p.at_ctx("role") && p.nth(1) == IDENT {
        p.bump_ctx();
        path(p);
    } else {
        expr(p);
    }
}

/// name [: type modifiers] [default value] [= formula] [modifier block]
fn field_rest(p: &mut Parser, _ctx: MemberCtx) {
    name(p);
    if p.eat(COLON) {
        type_(p);
        modifiers(p);
    }
    field_values(p);
    if p.at(NEWLINE) && p.nth(1) == INDENT {
        // modifier block: further modifiers and/or the value on following lines
        let b = p.start();
        p.bump();
        p.bump();
        while !p.at(DEDENT) && !p.at_eof() {
            if p.eat(NEWLINE) {
                continue;
            }
            let before = p.position();
            if p.at(EQ) || p.at(COLON_EQ) || p.at_ctx("default") {
                field_values(p);
            } else if !modifiers(p) {
                p.error("expected a modifier (`where`, `required when`…) or `= value`");
                recover_line(p);
                ensure_progress(p, before);
                continue;
            }
            end_line(p);
            ensure_progress(p, before);
        }
        p.eat(DEDENT);
        b.complete(p, MODIFIER_BLOCK);
        return;
    }
    end_line(p);
}

/// `default expr` (initial value) and/or `= expr` (formula).
fn field_values(p: &mut Parser) {
    loop {
        if p.at_ctx("default") {
            let m = p.start();
            p.bump_ctx();
            value_body(p);
            m.complete(p, DEFAULT_VALUE);
        } else if p.at(EQ) {
            value(p);
        } else if p.at(COLON_EQ) {
            p.error("formulas are written with `=`: `:=` is not used any more");
            value(p);
        } else {
            return;
        }
    }
}

/// Returns true if at least one modifier was parsed.
fn modifiers(p: &mut Parser) -> bool {
    let mut any = false;
    loop {
        let m = p.start();
        if p.eat(WHERE_KW) {
            // scalar shorthand: `where > 0`, `where >= self.min`
            if matches!(p.current(), GT | GE | LT | LE | EQ2 | NEQ) {
                p.bump();
            }
            expr(p);
        } else if p.at_ctx("required") && p.nth(1) == WHEN_KW {
            p.bump_ctx();
            p.bump();
            expr(p);
        } else if p.at_ctx("unique") || p.at_ctx("parent") || p.at_ctx("personal") {
            p.bump_ctx();
        } else if (p.at_ctx("expire") || p.at_ctx("retain")) && p.nth(1) == NUMBER {
            retention(p);
        } else if p.at_ctx("per") && p.nth_at_ctx(1, "company") {
            p.bump_ctx();
            p.bump_ctx();
        } else if p.at_ctx("via") || p.at_ctx("through") {
            p.bump_ctx();
            path(p);
        } else if (p.at_ctx("at") || p.at_ctx("format")) && p.nth(1) == STRING {
            // where a field of a record `from` a format is (`at "shipping.zip"`, `at "1..10"`)
            // and how its value is written there (`format "%d/%m/%Y"`)
            p.bump_ctx();
            p.bump();
        } else if p.at_ctx("decimals") && p.nth(1) == NUMBER {
            // `decimals 2`: the decimals a number is written with in a format
            p.bump_ctx();
            p.bump();
        } else if p.at_ctx("max") && p.nth(1) == NUMBER {
            // `max 10 MB`: the largest file a `File` field takes (KB, MB, GB)
            p.bump_ctx();
            p.bump();
            if p.at(IDENT) && matches!(p.nth_text(0), "B" | "KB" | "MB" | "GB") {
                p.bump_ctx();
            }
        } else if p.at_ctx("packed") || p.at_ctx("zoned") {
            // a number of an EBCDIC file as packed (COMP-3) or zoned decimal
            p.bump_ctx();
        } else {
            m.abandon(p);
            return any;
        }
        m.complete(p, MODIFIER);
        any = true;
    }
}

/// `expire 6 months after last_contact`, `retain 10 years after date[, then expire]`: a
/// number, a unit of time, the field the time is counted from.
fn retention(p: &mut Parser) {
    p.bump_ctx();
    p.expect(NUMBER);
    p.expect(IDENT);
    if p.at_ctx("after") {
        p.bump_ctx();
        path(p);
    } else {
        p.error("expected `after` and the field the time is counted from (`after created_at`)");
    }
    if p.at(COMMA) && p.nth_at_ctx(1, "then") {
        p.bump();
        p.bump_ctx();
        if p.at_ctx("expire") {
            p.bump_ctx();
        } else {
            p.error("expected `then expire`");
        }
    }
}

/// `= expr`, `:= expr`, or the same followed by an indented block on the next lines.
pub(crate) fn value(p: &mut Parser) {
    let m = p.start();
    p.bump(); // =
    value_body(p);
    m.complete(p, VALUE);
}

/// An expression on the same line, or an indented block whose last expression is the value.
fn value_body(p: &mut Parser) {
    if p.at(NEWLINE) && p.nth(1) == INDENT {
        let b = p.start();
        stmts::block(p);
        b.complete(p, BLOCK_EXPR);
    } else {
        expr(p);
    }
}

fn check(p: &mut Parser) {
    let m = p.start();
    let kind = if p.at_ctx("warn") { WARN } else { CHECK };
    p.bump_ctx();
    if p.at_ctx("per") && p.nth(1) == L_PAREN {
        p.bump_ctx();
        name_list(p, true);
        p.expect(COLON);
    }
    expr(p);
    if p.expect(COMMA) {
        expr(p);
    }
    end_line(p);
    m.complete(p, kind);
}

fn unique(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name_list(p, true);
    if p.eat(WHEN_KW) {
        expr(p);
    }
    end_line(p);
    m.complete(p, UNIQUE);
}

fn frozen(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    p.bump(); // when
    expr(p);
    loop {
        if p.at_ctx("except") || p.at_ctx("live") {
            p.bump_ctx();
            name_list(p, false);
        } else {
            break;
        }
    }
    end_line(p);
    m.complete(p, FROZEN);
}

/// `live a, b`: formulas of a frozen record that keep following the data (also in `extend`).
fn live(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name_list(p, false);
    end_line(p);
    m.complete(p, LIVE);
}

fn flow(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name_ref(p);
    p.expect(COLON);
    indented(p, |p| {
        let t = p.start();
        name_ref(p);
        while p.eat(PIPE) {
            name_ref(p);
        }
        p.expect(ARROW);
        name_ref(p);
        if p.at_ctx("by") {
            p.bump_ctx();
            name_ref(p);
        }
        if p.eat(WHEN_KW) {
            expr(p);
        }
        if p.at_ctx("unless") {
            p.bump_ctx();
            expr(p);
        }
        end_line(p);
        t.complete(p, TRANSITION);
    });
    m.complete(p, FLOW);
}

fn action(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    name(p);
    if p.at(L_PAREN) {
        param_list(p);
    }
    ret_type(p);
    p.expect(COLON);
    block(p);
    m.complete(p, ACTION);
}

fn post(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx(); // post
    p.bump_ctx(); // to
    path(p);
    if p.eat(WHEN_KW) {
        expr(p);
    }
    p.expect(COLON);
    block(p);
    m.complete(p, POST);
}

fn load(p: &mut Parser) {
    let m = p.start();
    p.bump_ctx();
    p.bump(); // from
    p.expect(STRING);
    end_line(p);
    m.complete(p, LOAD);
}
