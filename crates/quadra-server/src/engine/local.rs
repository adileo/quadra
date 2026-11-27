//! The engine without HTTP: what a request does once it is authenticated, called directly.
//! Benchmarks and tools measure and use exactly the code of `POST /v1/query`,
//! `GET /v1/records/{Entity}/{id}` and `POST /v1/tx`, without a server, a token or the rate
//! limit.

use super::*;
use quadra_store::auth::Principal;

pub struct Local {
    engine: Engine,
}

fn principal(user: u64) -> Principal {
    Principal {
        user_id: user,
        read_only: false,
        kind: "token".into(),
        csrf: None,
        name: "local".into(),
    }
}

fn answer(r: Result<Reply, Fail>) -> (u16, J) {
    match r {
        Ok(r) => (r.status, r.body),
        Err((s, m)) => (s, json!({ "error": m })),
    }
}

impl Local {
    /// The project at `root` over the database at `url` (initialized, schema current), with
    /// a record cache of `cache_mb` megabytes as `quadra serve` has.
    pub fn open(root: &std::path::Path, url: &str, cache_mb: usize) -> Result<Local, String> {
        let cfg = Config {
            root: root.to_path_buf(),
            url: url.to_string(),
            port: 0,
            dev_login: false,
            secure_cookies: false,
            cache_mb,
            prewarm_days: None,
            web: None,
            sweep_every: 0,
            editors: Vec::new(),
            watch: false,
            limits: Limits::none(),
            engines: Engines::one(),
        };
        Ok(Local {
            engine: Engine::new(&cfg)?,
        })
    }

    /// The same as `quadra dev` serves it: the apps of interface files are written back.
    pub fn open_dev(root: &std::path::Path, url: &str, cache_mb: usize) -> Result<Local, String> {
        let cfg = Config {
            root: root.to_path_buf(),
            url: url.to_string(),
            port: 0,
            dev_login: true,
            secure_cookies: false,
            cache_mb,
            prewarm_days: None,
            web: None,
            sweep_every: 0,
            editors: Vec::new(),
            watch: true,
            limits: Limits::none(),
            engines: Engines::one(),
        };
        Ok(Local {
            engine: Engine::new(&cfg)?,
        })
    }

    /// `quadra dev`: look at the files again, apply what changed. The event sent to the
    /// clients (`None`: nothing changed).
    pub fn reload(&mut self) -> Option<J> {
        self.engine.dev_reload()
    }

    /// `POST /v1/query` as `user`.
    pub fn query(&mut self, user: u64, q: &J) -> (u16, J) {
        answer(self.engine.query(&principal(user), q))
    }

    /// `GET /v1/model` as `user`: the entities, fields, actions and flows they may see.
    pub fn model(&mut self, user: u64) -> J {
        answer(self.engine.model(&principal(user))).1
    }

    /// `GET /v1/records/{entity}/{id}` as `user`.
    pub fn record(&mut self, user: u64, entity: &str, id: u64) -> (u16, J) {
        answer(self.engine.record(&principal(user), entity, id, None))
    }

    /// `GET /v1/search/{entity}?q=&limit=` as `user`.
    pub fn search(&mut self, user: u64, entity: &str, q: &str, limit: usize) -> (u16, J) {
        answer(self.engine.search(&principal(user), entity, q, limit, None))
    }

    /// `POST /v1/tx` as `user`, with an idempotency key.
    pub fn tx(&mut self, user: u64, body: &J, key: &str) -> (u16, J) {
        answer(self.engine.tx(&principal(user), body, key))
    }

    /// A call of `/v1/views…`, `/v1/teams…` or `/v1/directory` as `user` (`path` with its
    /// `/v1/` prefix, `query` the query string's pairs).
    pub fn views(
        &mut self,
        user: u64,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        body: &J,
    ) -> (u16, J) {
        let q: HashMap<String, String> = query
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        answer(
            self.engine
                .views_api(&principal(user), method, path, &q, body),
        )
    }

    /// `POST /v1/eval` as `user`: the formulas of a workbook.
    pub fn eval(&mut self, user: u64, body: &J) -> (u16, J) {
        answer(
            self.engine
                .views_api(&principal(user), "POST", "/v1/eval", &HashMap::new(), body),
        )
    }

    /// Show an app as the web client does on opening it (`apps::open_requests`): every
    /// component's first requests, as `user`, with the parameters' `values`. Returns what was
    /// asked and answered: `{requests, ms_engine, errors: ["component: …"], results: {component: [answers]}}`.
    pub fn open_app(&mut self, user: u64, cfg: &J, values: &J) -> J {
        self.show_app(user, cfg, values, false)
    }

    /// The same, for new values of the filters: what the client asks again (its components'
    /// evaluations and queries; not the model, not the buttons' records).
    pub fn refresh_app(&mut self, user: u64, cfg: &J, values: &J) -> J {
        self.show_app(user, cfg, values, true)
    }

    fn show_app(&mut self, user: u64, cfg: &J, values: &J, refresh: bool) -> J {
        let who = principal(user);
        let mut errors: Vec<String> = Vec::new();
        let mut results = serde_json::Map::new();
        let mut n = 0;
        let mut engine_ms = 0u64;
        let push = |results: &mut serde_json::Map<String, J>, c: &str, j: J| {
            results
                .entry(c.to_string())
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .unwrap()
                .push(j);
        };
        for r in super::apps::open_requests(cfg, values) {
            if refresh
                && matches!(
                    r,
                    super::apps::AppRequest::Model | super::apps::AppRequest::Record { .. }
                )
            {
                continue;
            }
            n += 1;
            match r {
                super::apps::AppRequest::Model => {
                    let (s, b) = answer(self.engine.model(&who));
                    if s != 200 {
                        errors.push(format!("model: {s} {b}"));
                    }
                }
                super::apps::AppRequest::Eval { component, body } => {
                    let (s, b) = answer(self.engine.views_api(
                        &who,
                        "POST",
                        "/v1/eval",
                        &HashMap::new(),
                        &body,
                    ));
                    if s != 200 {
                        errors.push(format!("{component}: {s} {b}"));
                        continue;
                    }
                    engine_ms += b["ms"].as_u64().unwrap_or(0);
                    for (k, c) in b["exprs"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .chain(b["cells"].as_object().into_iter().flatten())
                    {
                        if let Some(e) = c["error"].as_str() {
                            errors.push(format!(
                                "{component} {k}: {} {e}",
                                c["code"].as_str().unwrap_or("")
                            ));
                        }
                    }
                    push(&mut results, &component, b);
                }
                super::apps::AppRequest::Query {
                    component,
                    body,
                    columns,
                } => {
                    let (s, b) = answer(self.engine.query(&who, &body));
                    if s != 200 {
                        errors.push(format!("{component}: {s} {b}"));
                        continue;
                    }
                    if let Some(col) = columns {
                        // a board: the first cards of each column
                        for g in b["groups"].as_array().into_iter().flatten() {
                            let mut q = col.clone();
                            q.as_object_mut().unwrap().remove("__group_field");
                            q["group_filter"] = g["filter"].clone();
                            n += 1;
                            let (s, c) = answer(self.engine.query(&who, &q));
                            if s != 200 {
                                errors.push(format!("{component} column: {s} {c}"));
                            }
                        }
                    }
                    push(&mut results, &component, b);
                }
                super::apps::AppRequest::Record {
                    component,
                    entity,
                    id,
                } => {
                    let (s, b) = answer(self.engine.record(&who, &entity, id, None));
                    if s != 200 {
                        errors.push(format!("{component}: {s} {b}"));
                        continue;
                    }
                    push(&mut results, &component, b);
                }
            }
        }
        json!({ "requests": n, "ms_engine": engine_ms, "errors": errors, "results": results })
    }

    /// Forget the checked workbooks and the values of formulas kept (the next evaluation is
    /// cold).
    pub fn clear_sheets(&mut self) {
        self.engine.sheets = Default::default();
    }

    /// How many workbooks were checked so far (a check reused does not count).
    pub fn sheet_checks(&self) -> u64 {
        self.engine.sheets.checked
    }

    /// Empty the record cache (the next request reads its records from the database).
    pub fn clear_cache(&mut self) {
        self.engine.cache.borrow_mut().clear();
    }

    /// The record cache: (hits, misses, records held, bytes).
    pub fn cache(&self) -> (u64, u64, usize, usize) {
        let c = self.engine.cache.borrow();
        (c.hits, c.misses, c.len(), c.bytes())
    }
}
