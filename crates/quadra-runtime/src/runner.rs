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
                Ok(())
            }
            StmtKind::Expect(e) => self.expect(e, ctx),
            StmtKind::ExpectError { message, body } => {
                let mut got = None;
                for st in body {
                    if let Err(e) = self.statement(st, ctx) {
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
                Ok(())
            }
            Err(e) => Err(describe(&e)),
        }
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
        Ok(())
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
        let is_item = ["extend ", "fn ", "patch ", "entity "]
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
