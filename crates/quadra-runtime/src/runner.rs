//! Test runner: every `test` block runs on a fresh in-memory database.

use crate::interp::{Ctx, Flow, Interp, R};
use crate::value::V;
use quadra_ir::*;
use quadra_sema::{Diagnostic, Project, Severity};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct TestResult {
    pub name: String,
    pub location: String,
    pub passed: bool,
    pub message: Option<String>,
}

fn location(project: &Project, span: Span) -> String {
    let f = project.file(span.file);
    let line = f.text[..usize::from(span.range.start()).min(f.text.len())]
        .matches('\n')
        .count()
        + 1;
    let rel = f.path.strip_prefix(&project.root).unwrap_or(&f.path);
    format!("{}:{line}", rel.display())
}

/// Persistence used while running tests (a database per test).
pub trait TestBackend {
    /// Before a test: attach a commit sink (and formulas to materialize) to the interpreter.
    fn attach<'m>(
        &mut self,
        model: &'m Model,
        report: &'m quadra_types::Report,
        index: usize,
        interp: &mut Interp<'m>,
    ) -> Result<(), String>;
    /// Before each statement of a test: a fresh view of the committed data, if the backend
    /// loads on demand (like a new API request).
    fn refresh<'m>(&mut self, _interp: &mut Interp<'m>) -> Result<(), String> {
        Ok(())
    }
    /// After a test: reload from the database and compare with the interpreter's state.
    fn check<'m>(
        &mut self,
        model: &'m Model,
        report: &'m quadra_types::Report,
        index: usize,
        interp: &mut Interp<'m>,
    ) -> Result<(), String>;
}

/// Check the project and run its tests. Errors from the checks are returned instead.
pub fn run_tests(
    project: &Project,
    filter: Option<&str>,
) -> Result<Vec<TestResult>, Vec<Diagnostic>> {
    run_tests_with(project, filter, None)
}

pub fn run_tests_with(
    project: &Project,
    filter: Option<&str>,
    mut backend: Option<&mut dyn TestBackend>,
) -> Result<Vec<TestResult>, Vec<Diagnostic>> {
    let checked = quadra_analysis::check(project);
    if checked
        .diagnostics
        .iter()
        .any(|d| d.severity == Severity::Error)
    {
        return Err(checked.diagnostics);
    }
    let mut model = checked.model.expect("model");
    // the tests do not read the environment: the variables' `test` values, else defaults
    model.env = model.manifest.test_values();
    let report = checked.report;
    let mut results = Vec::new();
    for (index, test) in model.tests.iter().enumerate() {
        if filter.map_or(false, |f| !test.name.contains(f)) {
            continue;
        }
        let mut runner = Runner {
            project,
            model: &model,
            interp: Interp::new(&model),
            backend: backend.as_deref_mut(),
            bindings: HashMap::new(),
            received: Default::default(),
        };
        let mut outcome = Ok(());
        if let Some(b) = runner.backend.as_deref_mut() {
            outcome = b
                .attach(&model, &report, index, &mut runner.interp)
                .map_err(|e| format!("database: {e}"));
        }
        if outcome.is_ok() {
            outcome = runner.run(test);
        }
        let Runner {
            mut interp,
            backend: b,
            ..
        } = runner;
        if let Some(b) = b {
            let checked = b
                .check(&model, &report, index, &mut interp)
                .map_err(|e| format!("database: {e}"));
            if outcome.is_ok() {
                outcome = checked;
            }
        }
        results.push(TestResult {
            name: test.name.clone(),
            location: location(project, test.span),
            passed: outcome.is_ok(),
            message: outcome.err(),
        });
    }
    Ok(results)
}

/// Run one `test` block, then hand the interpreter to `inspect` (the sandbox: a session of
/// statements replayed on the current code). Statements run until the first failure, which
/// is returned with the data as it was at that point.
pub fn run_session<T>(
    project: &Project,
    name: &str,
    inspect: impl FnOnce(&Model, &quadra_types::Report, &mut Interp, &HashMap<String, V>) -> T,
) -> Result<(Option<String>, T), Vec<Diagnostic>> {
    let checked = quadra_analysis::check(project);
    if checked
        .diagnostics
        .iter()
        .any(|d| d.severity == Severity::Error)
    {
        return Err(checked.diagnostics);
    }
    let model = checked.model.expect("model");
    let Some(test) = model.tests.iter().find(|t| t.name == name).cloned() else {
        return Ok((
            Some(format!("no test named `{name}`")),
            inspect(
                &model,
                &checked.report,
                &mut Interp::new(&model),
                &HashMap::new(),
            ),
        ));
    };
    let mut runner = Runner {
        project,
        model: &model,
        interp: Interp::new(&model),
        backend: None,
        bindings: HashMap::new(),
        received: Default::default(),
    };
    let outcome = runner.run(&test);
    let Runner {
        mut interp,
        bindings,
        ..
    } = runner;
    let t = inspect(&model, &checked.report, &mut interp, &bindings);
    Ok((outcome.err(), t))
}

struct Runner<'a, 'b, 'c> {
    project: &'a Project,
    model: &'a Model,
    interp: Interp<'a>,
    backend: Option<&'b mut (dyn TestBackend + 'c)>,
    /// the variables of the test at its end (the sandbox refers to records by them)
    bindings: HashMap<String, V>,
    /// the keys of the messages received so far, per inbound: the same again is a duplicate
    received: std::collections::HashSet<(String, String)>,
}

impl<'a, 'b, 'c> Runner<'a, 'b, 'c> {
    fn refresh(&mut self) -> Result<(), String> {
        match self.backend.as_deref_mut() {
            Some(b) => b
                .refresh(&mut self.interp)
                .map_err(|e| format!("database: {e}")),
            None => Ok(()),
        }
    }

    fn at(&self, span: Span) -> String {
        location(self.project, span)
    }

    fn run(&mut self, test: &Block) -> Result<(), String> {
        self.interp.sent = Some(Vec::new());
        // seeds of the standard library and of the project
        let seeds = self.model.seeds.clone();
        self.interp.system += 1;
        let r = self.interp.transaction(|i| {
            for s in &seeds {
                let mut ctx = Ctx::default();
                i.stmts(&s.body, &mut ctx)?;
            }
            Ok(())
        });
        self.interp.system -= 1;
        r.map_err(|e| format!("seed failed: {}", describe(&e)))?;
        self.interp.load_units();
        let mut ctx = Ctx::default();
        ctx.scopes.push(Default::default());
        let mut outcome = Ok(());
        for s in &test.body {
            if let Err(e) = self.stmt(s, &mut ctx) {
                outcome = Err(e);
                break;
            }
        }
        self.bindings = ctx
            .scopes
            .iter()
            .flat_map(|s| s.iter().map(|(k, v)| (k.clone(), v.clone())))
            .collect();
        outcome
    }

    fn stmt(&mut self, s: &Stmt, ctx: &mut Ctx) -> Result<(), String> {
        match &s.kind {
            StmtKind::Use(name) => self.use_fixture(name, ctx),
            StmtKind::Clock(e) => {
                let v = self.interp.eval(e, ctx).map_err(|e| describe(&e))?;
                match v {
                    V::DateTime(dt) => self.interp.clock = dt,
                    // a date alone: 9:00 on the project's clock
                    V::Date(d) => {
                        self.interp.clock =
                            crate::time::instant(d.and_hms_opt(9, 0, 0).unwrap(), self.interp.tz)
                    }
                    other => return Err(format!("clock needs a date-time, found {other}")),
                }
                // the time passing runs the rules on the clock, as the server's sweep does when a
                // threshold passes: `becomes now() >= at + 24 hours` fires here, not at whatever
                // statement comes next (on a database, nothing else would look at that record)
                self.refresh()?;
                let at = self.at(s.span);
                let interp = &mut self.interp;
                interp.sweep = true;
                interp.system += 1;
                let r = interp.with_cause(format!("time-driven rules at clock {at}"), |i| {
                    i.transaction(|_| Ok(()))
                });
                interp.system -= 1;
                interp.sweep = false;
                interp.last_sweep = Some(interp.clock);
                r.map_err(|e| format!("{at}: {}", describe(&e)))?;
                // and the retentions that ended: values emptied, records taken away, each in a
                // transaction of its own (as the server's sweep does)
                self.interp
                    .expire_due()
                    .map(|_| ())
                    .map_err(|e| format!("{at}: {}", describe(&e)))
            }
            StmtKind::Expect(e) => self.expect(e, ctx),
            StmtKind::Receive {
                inbound,
                body,
                file,
            } => self
                .receive(inbound, body, *file, ctx)
                .map_err(|e| format!("{}: {e}", self.at(s.span))),
            StmtKind::Deliver {
                event,
                reply,
                failed,
            } => self
                .deliver(event, reply.as_ref(), failed.as_ref(), ctx)
                .map_err(|e| format!("{}: {e}", self.at(s.span))),
            StmtKind::ExpectSent { event, negated } => self
                .expect_sent(event, *negated, ctx)
                .map_err(|e| format!("{}: {e}", self.at(s.span))),
            StmtKind::ExpectError { message, body } => {
                let mut got = None;
                for st in body {
                    let r = match st.kind {
                        // the statements of a test, run by the runner
                        StmtKind::Receive { .. }
                        | StmtKind::Deliver { .. }
                        | StmtKind::ExpectSent { .. } => self.stmt(st, ctx),
                        _ => self.statement(st, ctx),
                    };
                    if let Err(e) = r {
                        got = Some(e);
                        break;
                    }
                }
                match got {
                    None => Err(format!(
                        "{}: expected an error containing {message:?}, but it succeeded",
                        self.at(s.span)
                    )),
                    Some(e) if e.contains(message.as_str()) => Ok(()),
                    Some(e) => Err(format!(
                        "{}: expected an error containing {message:?}, got: {e}",
                        self.at(s.span)
                    )),
                }
            }
            StmtKind::ExpectCompileError { message, source } => {
                self.expect_compile_error(message, source, s.span)
            }
            StmtKind::As { user, body } => {
                let saved = self.interp.user;
                self.interp.user = self.login(user);
                if self.interp.user.is_none() {
                    return Err(format!("{}: unknown user `{user}`", self.at(s.span)));
                }
                let mut r = Ok(());
                for st in body {
                    r = self.stmt(st, ctx);
                    if r.is_err() {
                        break;
                    }
                }
                self.interp.user = saved;
                r
            }
            _ => self
                .statement(s, ctx)
                .map_err(|e| format!("{}: {e}", self.at(s.span))),
        }
    }

    /// `receive OrderIn "…"`: the message read as the inbound's record and handled by its code,
    /// as the system, in a transaction of its own (a message with a key already received is a
    /// duplicate: nothing happens). A message that is not the record, or code that fails, fails
    /// the statement: the message would be `rejected`.
    fn receive(
        &mut self,
        name: &str,
        body: &Expr,
        file: bool,
        ctx: &mut Ctx,
    ) -> Result<(), String> {
        self.refresh()?;
        let model = self.model;
        let Some(i) = model.inbounds.iter().find(|i| i.name == name) else {
            return Err(format!("no inbound `{name}`"));
        };
        let v = self.interp.eval(body, ctx).map_err(|e| describe(&e))?;
        let bytes = if file {
            let path = self.project.root.join(v.to_string());
            std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?
        } else {
            v.to_string().into_bytes()
        };
        // a file of rows: its rows, all in one transaction (`atomic per file`) or each in its
        // own (`atomic per row`, the statement failing only when every row is refused)
        if model
            .entity(&i.ty)
            .is_some_and(|e| matches!(e.format.as_deref(), Some("csv" | "fixed")))
        {
            return self.receive_rows(i, &bytes);
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| "rejected: the message is not text (UTF-8)".to_string())?;
        let doc = crate::formats::parse(model, &i.ty, &text)
            .map_err(|e| format!("rejected: the message is not a {}: {e}", i.ty))?;
        if let Some(k) = &i.key {
            let mut kctx = Ctx::default();
            kctx.bind(&i.binding, doc.clone());
            let key = self
                .interp
                .eval(k, &mut kctx)
                .map_err(|e| describe(&e))?
                .to_string();
            if !self.received.insert((name.to_string(), key)) {
                return Ok(());
            }
        }
        let n = self.received.len();
        self.interp.system += 1;
        let r = self
            .interp
            .with_cause(format!("inbound:{name}#test{n} handled"), |it| {
                it.transaction(|it| {
                    let mut c = Ctx::default();
                    c.bind(&i.binding, doc.clone());
                    match it.stmts(&i.body, &mut c) {
                        Ok(()) | Err(Flow::Return(_)) => Ok(()),
                        Err(e) => Err(e),
                    }
                })
            });
        self.interp.system -= 1;
        r.map_err(|e| format!("rejected: {}", describe(&e)))
    }

    /// `receive` of a file of rows (`from csv`, `from fixed`).
    fn receive_rows(&mut self, i: &quadra_ir::Inbound, bytes: &[u8]) -> Result<(), String> {
        let each = crate::formats::read_rows(self.model, &i.ty, bytes)
            .map_err(|e| format!("rejected: the file is not a file of {}: {e}", i.ty))?;
        let per_row = i.atomic == Some(quadra_ir::Atomic::Row);
        let mut rows = Vec::new();
        let mut refused = Vec::new();
        for (n, r) in each {
            match r {
                Ok(v) => rows.push((n, v)),
                Err(e) if per_row => refused.push(e),
                Err(e) => return Err(format!("rejected: {e}")),
            }
        }
        let n = self.received.len();
        self.received.insert((i.name.clone(), format!("#file{n}")));
        let groups: Vec<Vec<(usize, V)>> = if per_row {
            rows.into_iter().map(|r| vec![r]).collect()
        } else {
            vec![rows]
        };
        let mut committed = 0;
        self.interp.system += 1;
        for (k, group) in groups.into_iter().enumerate() {
            let line = std::cell::Cell::new(0);
            let r =
                self.interp
                    .with_cause(format!("inbound:{}#test{n}/{k} handled", i.name), |it| {
                        it.transaction(|it| {
                            for (l, row) in &group {
                                line.set(*l);
                                let mut c = Ctx::default();
                                c.bind(&i.binding, row.clone());
                                match it.stmts(&i.body, &mut c) {
                                    Ok(()) | Err(Flow::Return(_)) => {}
                                    Err(e) => return Err(e),
                                }
                            }
                            Ok(())
                        })
                    });
            match r {
                Ok(()) => committed += 1,
                Err(e) if per_row => refused.push(format!("line {}: {}", line.get(), describe(&e))),
                Err(e) => {
                    self.interp.system -= 1;
                    return Err(format!("rejected: line {}: {}", line.get(), describe(&e)));
                }
            }
        }
        self.interp.system -= 1;
        if per_row && committed == 0 && !refused.is_empty() {
            return Err(format!("rejected: every row was refused: {}", refused[0]));
        }
        Ok(())
    }

    /// `deliver OrderPlaced [reply …|failed "…"]`: what the other side would do with the events
    /// of `event` waiting in the outbox; each outcome runs its reactions in a transaction of its
    /// own, as the system.
    fn deliver(
        &mut self,
        event: &str,
        reply: Option<&Expr>,
        failed: Option<&Expr>,
        ctx: &mut Ctx,
    ) -> Result<(), String> {
        self.refresh()?;
        let reply = reply
            .map(|r| self.interp.eval(r, ctx))
            .transpose()
            .map_err(|e| describe(&e))?;
        let error = failed
            .map(|f| self.interp.eval(f, ctx))
            .transpose()
            .map_err(|e| describe(&e))?
            .map(|v| v.to_string());
        let waiting: Vec<(usize, V)> = self
            .interp
            .sent
            .iter()
            .flatten()
            .enumerate()
            .filter(|(_, (v, pending))| {
                *pending && matches!(v, V::Event { name, .. } if &**name == event)
            })
            .map(|(i, (v, _))| (i, v.clone()))
            .collect();
        if waiting.is_empty() {
            return Err(format!("no {event} waiting to be delivered"));
        }
        let has_reply = self
            .model
            .outbounds
            .iter()
            .find(|o| o.event == event)
            .is_some_and(|o| o.reply.is_some());
        for (i, v) in waiting {
            if let Some(sent) = &mut self.interp.sent {
                sent[i].1 = false;
            }
            let V::Event { fields, .. } = &v else {
                continue;
            };
            let text = |s: &str| V::Text(std::rc::Rc::from(s));
            let mut parts = vec![
                (
                    Some("event".to_string()),
                    V::Tuple(std::rc::Rc::new(
                        fields
                            .iter()
                            .map(|(k, v)| (Some(k.to_string()), v.clone()))
                            .collect(),
                    )),
                ),
                (Some("at".to_string()), V::DateTime(self.interp.clock)),
                (Some("attempts".to_string()), V::Int(1)),
                (
                    Some("status".to_string()),
                    if error.is_some() {
                        V::None
                    } else {
                        V::Int(200)
                    },
                ),
                (Some("response".to_string()), V::None),
            ];
            if let Some(e) = &error {
                parts.push((Some("error".to_string()), text(e)));
            }
            if has_reply {
                parts.push((Some("reply".to_string()), reply.clone().unwrap_or(V::None)));
            }
            let outcome = V::Tuple(std::rc::Rc::new(parts));
            let delivered = error.is_none();
            self.interp.system += 1;
            let r = self.interp.with_cause(
                format!(
                    "outbound:{event}#test{i} {}",
                    if delivered { "delivered" } else { "failed" }
                ),
                |it| it.transaction(|it| it.run_outcome(event, delivered, &outcome)),
            );
            self.interp.system -= 1;
            r.map_err(|e| describe(&e))?;
        }
        Ok(())
    }

    /// `expect sent OrderPlaced(order=o)`: an event of that name with those fields was emitted
    /// (or, `negated`, none was).
    fn expect_sent(&mut self, event: &Expr, negated: bool, ctx: &mut Ctx) -> Result<(), String> {
        self.refresh()?;
        let (name, args): (&str, &[Arg]) = match &event.kind {
            ExprKind::Call { callee, args } => match &callee.kind {
                ExprKind::Name(n) => (n.as_str(), args.as_slice()),
                _ => return Err("`expect sent` takes an event".into()),
            },
            ExprKind::Name(n) => (n.as_str(), &[]),
            _ => return Err("`expect sent` takes an event".into()),
        };
        let mut want = Vec::new();
        for a in args {
            let v = self.interp.eval(&a.value, ctx).map_err(|e| describe(&e))?;
            want.push((a.name.clone().unwrap_or_default(), v));
        }
        let events: Vec<V> = self
            .interp
            .sent
            .iter()
            .flatten()
            .filter(|(v, _)| matches!(v, V::Event { name: n, .. } if &**n == name))
            .map(|(v, _)| v.clone())
            .collect();
        let found = events.iter().any(|v| {
            let V::Event { fields, .. } = v else {
                return false;
            };
            want.iter()
                .all(|(k, w)| fields.iter().any(|(f, x)| &**f == k.as_str() && x == w))
        });
        if found != negated {
            return Ok(());
        }
        let shown: Vec<String> = events
            .iter()
            .map(|v| match v {
                V::Event { name, fields } => {
                    let parts: Vec<String> = fields
                        .iter()
                        .map(|(k, x)| format!("{k}={}", self.interp.display(x)))
                        .collect();
                    format!("{name}({})", parts.join(", "))
                }
                other => self.interp.display(other),
            })
            .collect();
        Err(format!(
            "expected {} {}: {}",
            if negated { "no" } else { "an event like" },
            print::expr(event),
            if shown.is_empty() {
                format!("no {name} was sent")
            } else {
                format!("sent: {}", shown.join(", "))
            }
        ))
    }

    fn login(&mut self, login: &str) -> Option<u64> {
        let ids = self.interp.ids("User", None);
        ids.into_iter().find(|id| {
            self.interp
                .store
                .read(*id, "login", None)
                .map_or(false, |v| v.to_string() == login)
        })
    }

    /// One statement = one transaction.
    fn statement(&mut self, s: &Stmt, ctx: &mut Ctx) -> Result<(), String> {
        self.refresh()?;
        let mut local = ctx.clone();
        let r = self
            .interp
            .with_cause(format!("test statement at {}", self.at(s.span)), |i| {
                i.transaction(|i| match i.stmt(s, &mut local) {
                    Ok(()) | Err(Flow::Return(_)) => Ok(()),
                    Err(e) => Err(e),
                })
            });
        match r {
            Ok(()) => {
                *ctx = local;
                self.attach_due()
            }
            Err(e) => Err(describe(&e)),
        }
    }

    /// After a commit, as the server does: the documents of the `attach`es due rendered, and
    /// their records given `sha256:<hex>` of them, each in a transaction of its own.
    fn attach_due(&mut self) -> Result<(), String> {
        let model = self.model;
        for a in &model.attaches {
            for id in self.interp.ids(&a.entity, None) {
                if !self
                    .interp
                    .store
                    .read(id, &a.field, None)
                    .unwrap_or(V::None)
                    .is_none()
                {
                    continue;
                }
                let rec = V::Ref {
                    ent: std::rc::Rc::from(a.entity.as_str()),
                    id,
                };
                if let Some(w) = &a.when {
                    let mut c = Ctx {
                        this: Some(rec.clone()),
                        entity: Some(a.entity.clone()),
                        ..Ctx::default()
                    };
                    self.interp.system += 1;
                    let v = self.interp.eval(w, &mut c);
                    self.interp.system -= 1;
                    match v {
                        Ok(V::Bool(true)) => {}
                        Ok(_) => continue,
                        Err(e) => {
                            return Err(format!(
                                "attach {}: its `when`: {}",
                                a.document,
                                describe(&e)
                            ))
                        }
                    }
                }
                let doc = self
                    .interp
                    .render_document(&a.document, vec![rec.clone()], false)
                    .and_then(|d| match &a.signer {
                        Some(by) => self
                            .interp
                            .sign_call("sign", vec![d, crate::value::text(by)]),
                        None => Ok(d),
                    })
                    .map_err(|e| {
                        format!(
                            "attach {} to {}#{id}: {}",
                            a.document,
                            a.entity,
                            describe(&e)
                        )
                    })?;
                let hash = match &doc {
                    V::Tuple(t) => t
                        .iter()
                        .find(|(k, _)| k.as_deref() == Some("hash"))
                        .map(|(_, v)| v.to_string())
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                let value = crate::value::text(&format!("sha256:{}", hash.trim_matches('"')));
                self.interp.system += 1;
                let r = self
                    .interp
                    .with_cause(format!("attach:{} {}#{id}", a.document, a.entity), |i| {
                        i.transaction(|i| i.set(&rec, &a.field, value.clone(), None))
                    });
                self.interp.system -= 1;
                r.map_err(|e| {
                    format!(
                        "attach {} to {}#{id}: {}",
                        a.document,
                        a.entity,
                        describe(&e)
                    )
                })?;
            }
        }
        Ok(())
    }

    fn use_fixture(&mut self, name: &str, ctx: &mut Ctx) -> Result<(), String> {
        let mut chain = Vec::new();
        let mut cur = Some(name.to_string());
        while let Some(n) = cur {
            let Some(f) = self.model.fixtures.iter().find(|f| f.name == n) else {
                return Err(format!("unknown fixture `{n}`"));
            };
            chain.push(f);
            cur = f.extends.clone();
        }
        chain.reverse();
        self.refresh()?;
        // a fixture is one transaction: its records may refer to each other
        let mut fctx = ctx.clone();
        let interp = &mut self.interp;
        interp.system += 1;
        let r: R<()> = interp.transaction(|i| {
            for f in &chain {
                for st in &f.body {
                    match &st.kind {
                        StmtKind::Clock(e) => {
                            if let V::DateTime(dt) = i.eval(e, &mut fctx)? {
                                i.clock = dt;
                            }
                        }
                        _ => i.stmt(st, &mut fctx)?,
                    }
                }
            }
            Ok(())
        });
        interp.system -= 1;
        r.map_err(|e| format!("fixture `{name}` failed: {}", describe(&e)))?;
        *ctx = fctx;
        self.attach_due()
    }

    fn expect(&mut self, e: &Expr, ctx: &mut Ctx) -> Result<(), String> {
        self.refresh()?;
        let ok = self
            .interp
            .eval(e, ctx)
            .map_err(|err| format!("{}: {}", self.at(e.span), describe(&err)))?;
        if ok.truthy() {
            return Ok(());
        }
        let detail = match &e.kind {
            ExprKind::Binary { lhs, rhs, .. } => {
                let l = self
                    .interp
                    .eval(lhs, ctx)
                    .map(|v| self.interp.display(&v))
                    .unwrap_or_default();
                let r = self
                    .interp
                    .eval(rhs, ctx)
                    .map(|v| self.interp.display(&v))
                    .unwrap_or_default();
                format!("\n      left:  {l}\n      right: {r}")
            }
            _ => String::new(),
        };
        Err(format!(
            "{}: expectation failed: {}{detail}",
            self.at(e.span),
            print::expr(e)
        ))
    }

    fn expect_compile_error(&self, message: &str, source: &str, span: Span) -> Result<(), String> {
        let is_item = ["extend ", "fn ", "patch ", "entity ", "record "]
            .iter()
            .any(|k| source.starts_with(k));
        let text = if is_item {
            format!("{source}\n")
        } else {
            format!(
                "fn __compile_check():\n{}",
                source
                    .lines()
                    .map(|l| format!("    {l}\n"))
                    .collect::<String>()
            )
        };
        let mut sources: Vec<(PathBuf, String)> = self
            .project
            .user_files()
            .map(|f| (f.path.clone(), f.text.clone()))
            .collect();
        sources.push((self.project.root.join("tests/__compile_check.qd"), text));
        let p = Project::from_sources(&self.project.root, sources);
        let checked = quadra_analysis::check(&p);
        let errors: Vec<String> = checked
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| d.message.clone())
            .collect();
        if errors.iter().any(|e| e.contains(message)) {
            Ok(())
        } else if errors.is_empty() {
            Err(format!(
                "{}: expected a compile error containing {message:?}, but it compiles",
                self.at(span)
            ))
        } else {
            Err(format!(
                "{}: expected a compile error containing {message:?}, got: {}",
                self.at(span),
                errors.join(" | ")
            ))
        }
    }
}

fn describe(f: &Flow) -> String {
    match f {
        Flow::Fail(m) => m.clone(),
        Flow::Return(_) => "unexpected return".into(),
        Flow::Conflict(c) => format!(
            "conflict: {}",
            c.iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ),
    }
}
