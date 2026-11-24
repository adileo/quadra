//! Canonical text of IR expressions and types: used in messages, `quadra trace` and the
//! fingerprints of certified metrics (formatting and comments do not change it).

use crate::*;

pub fn ty(t: &Ty) -> String {
    match t {
        Ty::Path { segments, args, .. } => {
            let base = segments.join(".");
            if args.is_empty() {
                base
            } else {
                format!("{base}<{}>", args.iter().map(ty).collect::<Vec<_>>().join(", "))
            }
        }
        Ty::Optional(i) => format!("{}?", ty(i)),
        Ty::List(i) => format!("[{}]", ty(i)),
        Ty::Set(i) => format!("{{{}}}", ty(i)),
        Ty::Tuple(fs) => format!("({})", fs.iter().map(|(n, t)| format!("{n}: {}", ty(t))).collect::<Vec<_>>().join(", ")),
        Ty::Enum(vs) => format!("enum({})", vs.iter().map(variant).collect::<Vec<_>>().join(", ")),
        Ty::Union(ts) => ts.iter().map(ty).collect::<Vec<_>>().join(" | "),
        Ty::Error => "?".into(),
    }
}

fn variant(v: &Variant) -> String {
    if v.fields.is_empty() {
        v.name.clone()
    } else {
        format!("{}({})", v.name, v.fields.iter().map(|(n, t)| format!("{n}: {}", ty(t))).collect::<Vec<_>>().join(", "))
    }
}

pub fn lit(l: &Lit) -> String {
    match l {
        Lit::Number(n) => n.clone(),
        Lit::Percent(p) => format!("{p}%"),
        Lit::Str(s) => format!("{s:?}"),
        Lit::Date(d) | Lit::DateTime(d) | Lit::YearMonth(d) => d.clone(),
        Lit::Bool(b) => b.to_string(),
        Lit::None => "none".into(),
    }
}

pub fn binop(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "and",
        BinOp::Or => "or",
        BinOp::Implies => "implies",
        BinOp::Coalesce => "??",
        BinOp::In => "in",
        BinOp::NotIn => "not in",
        BinOp::Is => "is",
        BinOp::IsNot => "is not",
        BinOp::Range => "..",
    }
}

fn pattern(p: &Pattern) -> String {
    match p {
        Pattern::Wildcard => "_".into(),
        Pattern::Lit(l) => lit(l),
        Pattern::Name(n) => n.clone(),
        Pattern::Ctor { name, args } => format!("{name}({})", args.iter().map(pattern).collect::<Vec<_>>().join(", ")),
    }
}

pub fn expr(e: &Expr) -> String {
    match &e.kind {
        ExprKind::Lit(l) => lit(l),
        ExprKind::FString(parts) => {
            let mut s = String::from("f\"");
            for p in parts {
                match p {
                    FPart::Text(t) => s.push_str(&t.replace('{', "{{").replace('}', "}}")),
                    FPart::Expr(e, None) => s.push_str(&format!("{{{}}}", expr(e))),
                    FPart::Expr(e, Some(spec)) => s.push_str(&format!("{{{}:{spec}}}", expr(e))),
                }
            }
            s.push('"');
            s
        }
        ExprKind::Quantity { value, unit, per } => match per {
            Some(p) => format!("{value} {unit}/{p}"),
            None => format!("{value} {unit}"),
        },
        ExprKind::Name(n) => n.clone(),
        ExprKind::SelfRef => "self".into(),
        ExprKind::Field { base, name, optional } => format!("{}{}{name}", atom(base), if *optional { "?." } else { "." }),
        ExprKind::Call { callee, args } => format!("{}({})", atom(callee), args.iter().map(arg).collect::<Vec<_>>().join(", ")),
        ExprKind::Index { base, index } => format!("{}[{}]", expr(base), expr(index)),
        ExprKind::Unary { op: UnOp::Neg, expr: x } => format!("-{}", atom(x)),
        ExprKind::Unary { op: UnOp::Not, expr: x } => format!("not {}", atom(x)),
        ExprKind::Binary { op, lhs, rhs } => format!("{} {} {}", atom(lhs), binop(*op), atom(rhs)),
        ExprKind::If { cond, then, otherwise } => format!("{} if {} else {}", atom(then), atom(cond), atom(otherwise)),
        ExprKind::Lambda { params, body } => {
            if params.len() == 1 {
                format!("{} => {}", params[0], expr(body))
            } else {
                format!("({}) => {}", params.join(", "), expr(body))
            }
        }
        ExprKind::Tuple(items) => format!(
            "({})",
            items
                .iter()
                .map(|(n, e)| match n {
                    Some(n) => format!("{n}: {}", expr(e)),
                    None => expr(e),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ExprKind::List(items) => {
            format!("[{}]", items.iter().map(arg).collect::<Vec<_>>().join(", "))
        }
        ExprKind::Set(items) => format!("{{{}}}", items.iter().map(expr).collect::<Vec<_>>().join(", ")),
        ExprKind::When(arms) => format!("when: {}", arms.iter().map(|(c, v)| format!("{} => {}", c.as_ref().map(expr).unwrap_or_else(|| "else".into()), expr(v))).collect::<Vec<_>>().join("; ")),
        ExprKind::Match { subject, arms } => {
            format!("match {}: {}", expr(subject), arms.iter().map(|a| format!("{} => {}", a.patterns.iter().map(pattern).collect::<Vec<_>>().join(" | "), expr(&a.body))).collect::<Vec<_>>().join("; "))
        }
        ExprKind::Block(stmts) => format!("{{ {} }}", stmts.iter().map(stmt).collect::<Vec<_>>().join("; ")),
        ExprKind::As { user, body } => format!("as {user}: {}", expr(body)),
        ExprKind::Fail(x) => format!("fail {}", expr(x)),
        ExprKind::Error => "<error>".into(),
    }
}

fn atom(e: &Expr) -> String {
    match &e.kind {
        ExprKind::Binary { .. } | ExprKind::If { .. } | ExprKind::Lambda { .. } | ExprKind::Unary { .. } => format!("({})", expr(e)),
        _ => expr(e),
    }
}

fn arg(a: &Arg) -> String {
    let v = expr(&a.value);
    match (&a.name, a.spread) {
        (_, true) => format!("*{v}"),
        (Some(n), _) => format!("{n}={v}"),
        _ => v,
    }
}

pub fn stmt(s: &Stmt) -> String {
    match &s.kind {
        StmtKind::Let { name, value, .. } => format!("let {name} = {}", expr(value)),
        StmtKind::Return(Some(e)) => format!("return {}", expr(e)),
        StmtKind::Return(None) => "return".into(),
        StmtKind::Expr(e) => expr(e),
        StmtKind::Assign { target, value, .. } => format!("{} = {}", expr(target), expr(value)),
        StmtKind::Require { cond, .. } => format!("require {}", expr(cond)),
        StmtKind::Fail(e) => format!("fail {}", expr(e)),
        StmtKind::If { cond, then, otherwise, .. } => format!(
            "if {}: {}{}",
            expr(cond),
            then.iter().map(stmt).collect::<Vec<_>>().join("; "),
            otherwise.as_ref().map(|o| format!(" else: {}", o.iter().map(stmt).collect::<Vec<_>>().join("; "))).unwrap_or_default()
        ),
        StmtKind::For { names, iter, body } => {
            format!("for {} in {}: {}", names.join(", "), expr(iter), body.iter().map(stmt).collect::<Vec<_>>().join("; "))
        }
        _ => format!("{:?}", std::mem::discriminant(&s.kind)),
    }
}
