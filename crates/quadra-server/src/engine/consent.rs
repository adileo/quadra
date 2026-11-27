//! OAuth2 with a user's consent, on the engine's side (`crate::oauth` has the protocol): the
//! tokens read from `_quadra_oauth` and handed to the requests that need them, those refreshed
//! by the host written back, the authorization started by an admin and finished by the
//! provider's callback, and what the studio shows of it.
//!
//! - `GET /v1/connections`: each connection, and for one with consent whether it is connected,
//!   its scope, when its token expires, who connected it and when, the last error
//! - `GET /v1/connections/{name}/authorize`: a redirect to the provider (admins only)
//! - `POST /v1/connections/{name}/disconnect`: its tokens forgotten (admins only)

use super::{reply, Engine, Fail, Reply};
use crate::oauth;
use quadra_sema::manifest::Setting;
use quadra_store::auth::Principal;
use serde_json::{json, Value as J};
use std::collections::HashMap;

impl Engine {
    fn setting_value(&self, s: &Setting) -> Result<String, String> {
        let model = self
            .checked
            .model
            .as_ref()
            .ok_or("the project does not check")?;
        match s {
            Setting::Literal(l) => Ok(l.clone()),
            Setting::Var(v) => model
                .manifest
                .value(&self.env, v)
                .map(str::to_string)
                .ok_or_else(|| format!("the variable {v} is not set where the app runs")),
        }
    }

    /// The tokens kept for a request of a connection with consent.
    pub(crate) fn fill_consent(&mut self, u: &mut oauth::Use) {
        u.key = oauth::cache_key(&self.url, &u.connection);
        let row = match quadra_store::grants::get(&mut self.conn, &u.connection) {
            Ok(r) => r,
            Err(e) => {
                u.problem = Some(format!("the tokens of `{}`: {e}", u.connection));
                return;
            }
        };
        let Some(row) = row else { return };
        if let Some(e) = row.error {
            u.problem = Some(e);
            return;
        }
        let Some(sealed) = row.sealed else { return };
        match oauth::key().and_then(|k| oauth::Tokens::open(&k, &u.connection, &sealed)) {
            Ok(t) => u.stored = Some(t),
            Err(e) => {
                u.problem = Some(format!(
                    "the tokens of `{}` cannot be read ({e}): an admin connects it again at /v1/connections/{}/authorize",
                    u.connection, u.connection
                ))
            }
        }
    }

    /// The tokens a pull of `connection` uses, if it is a connection with consent.
    pub(crate) fn consent_of(&mut self, connection: &str) -> Option<oauth::Use> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref()?;
        let c = model.manifest.connection(connection)?.consent()?.clone();
        let value = |s: &Setting| self.setting_value(s);
        let mut u = oauth::Use {
            key: String::new(),
            connection: connection.to_string(),
            token_url: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            in_body: c.in_body,
            stored: None,
            problem: None,
        };
        match (
            value(&c.token_url),
            value(&c.client_id),
            value(&Setting::Var(c.client_secret.clone())),
        ) {
            (Ok(t), Ok(i), Ok(s)) => {
                u.token_url = t;
                u.client_id = i;
                u.client_secret = s;
                self.fill_consent(&mut u);
            }
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => u.problem = Some(e),
        }
        Some(u)
    }

    /// The tokens refreshed in this process since, written to the database.
    pub(crate) fn save_consents(&mut self) {
        let changed = oauth::unsaved(&self.url);
        if changed.is_empty() {
            return;
        }
        let Ok(key) = oauth::key() else { return };
        let at = super::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        for (connection, tokens, error, version) in changed {
            let mut row = quadra_store::grants::get(&mut self.conn, &connection)
                .ok()
                .flatten()
                .unwrap_or_else(|| quadra_store::grants::Grant {
                    connection: connection.clone(),
                    ..Default::default()
                });
            if let Some(t) = &tokens {
                row.sealed = Some(t.seal(&key, &connection));
                row.scope = t.scope.clone().or(row.scope);
                row.expires_at = t.expires_at.map(stamp);
                row.refreshed_at = Some(at.clone());
            }
            row.error = error;
            match self.side(move |c| quadra_store::grants::put(c, &row)) {
                Ok(()) => oauth::saved(&self.url, &connection, version),
                Err(e) => eprintln!("oauth: the tokens of `{connection}` were not written: {e}"),
            }
        }
    }

    /// A step of the provider's callback (`/oauth/{connection}/callback`).
    pub(crate) fn consent_step(&mut self, step: oauth::Step) -> Result<oauth::Next, String> {
        let checked = self.checked.clone();
        let model = checked.model.as_ref().ok_or("the project does not check")?;
        let consent = |name: &str| {
            model
                .manifest
                .connection(name)
                .and_then(|c| c.consent())
                .cloned()
                .ok_or_else(|| format!("`{name}` is not a connection with OAuth2 consent"))
        };
        match step {
            oauth::Step::Open { connection, state } => {
                let c = consent(&connection)?;
                let key = oauth::key()?;
                let state = oauth::State::open(&key, &connection, &state)?;
                let client = oauth::Client {
                    token_url: self.setting_value(&c.token_url)?,
                    client_id: self.setting_value(&c.client_id)?,
                    client_secret: self.setting_value(&Setting::Var(c.client_secret.clone()))?,
                    in_body: c.in_body,
                };
                Ok(oauth::Next::Exchange { client, state })
            }
            oauth::Step::Keep {
                connection,
                tokens,
                by,
            } => {
                let c = consent(&connection)?;
                let key = oauth::key()?;
                let at = super::now().format("%Y-%m-%dT%H:%M:%S").to_string();
                let row = quadra_store::grants::Grant {
                    connection: connection.clone(),
                    sealed: Some(tokens.seal(&key, &connection)),
                    scope: tokens.scope.clone().or(c.scope.clone()),
                    expires_at: tokens.expires_at.map(stamp),
                    connected_at: Some(at),
                    connected_by: Some(by),
                    refreshed_at: None,
                    error: None,
                };
                self.side(move |cn| quadra_store::grants::put(cn, &row))?;
                oauth::replace(
                    &oauth::cache_key(&self.url, &connection),
                    &connection,
                    Some(tokens),
                );
                Ok(oauth::Next::Kept)
            }
        }
    }

    /// What the studio shows of the connections: for one with consent, its state, never its
    /// tokens.
    pub(crate) fn connections_status(&mut self) -> Vec<J> {
        let checked = self.checked.clone();
        let Some(model) = checked.model.as_ref() else {
            return Vec::new();
        };
        let rows: HashMap<String, quadra_store::grants::Grant> =
            quadra_store::grants::all(&mut self.conn)
                .unwrap_or_default()
                .into_iter()
                .map(|g| (g.connection.clone(), g))
                .collect();
        let key = oauth::key().err();
        model
            .manifest
            .connections
            .iter()
            .map(|c| {
                let mut j = json!({
                    "name": c.name,
                    "kind": c.kind_name(),
                    "location": c.agent.as_ref().map(|a| format!("agent:{a}")),
                });
                if let Some(consent) = c.consent() {
                    let r = rows.get(&c.name);
                    let z = |s: &Option<String>| s.as_ref().map(|s| format!("{s}Z"));
                    j["oauth2"] = json!({
                        "grant": "authorization_code",
                        "connected": r.is_some_and(|r| r.sealed.is_some() && r.error.is_none()),
                        "scope": r.and_then(|r| r.scope.clone()).or(consent.scope.clone()),
                        "expires_at": r.and_then(|r| z(&r.expires_at)),
                        "connected_at": r.and_then(|r| z(&r.connected_at)),
                        "connected_by": r.and_then(|r| r.connected_by.clone()),
                        "refreshed_at": r.and_then(|r| z(&r.refreshed_at)),
                        "error": r.and_then(|r| r.error.clone()).or(key.clone()),
                        "authorize": format!("/v1/connections/{}/authorize", c.name),
                    });
                }
                j
            })
            .collect()
    }

    /// `/v1/connections…` (see the module).
    pub(crate) fn connections_api(
        &mut self,
        who: &Principal,
        path: &str,
        query: &HashMap<String, String>,
    ) -> Result<Reply, Fail> {
        if !self.who(who)?.admin {
            return Err((
                403,
                "only who may edit `User.roles` manages the connections".into(),
            ));
        }
        let parts: Vec<&str> = path
            .trim_start_matches("/v1/connections")
            .split('/')
            .filter(|p| !p.is_empty())
            .collect();
        match parts.as_slice() {
            [] => Ok(reply(
                200,
                json!({ "connections": self.connections_status() }),
            )),
            [name, "authorize"] => {
                let checked = self.checked.clone();
                let model = checked
                    .model
                    .as_ref()
                    .ok_or((503, "the project does not check".to_string()))?;
                let c = model
                    .manifest
                    .connection(name)
                    .ok_or((404, format!("no connection `{name}`")))?;
                let consent = c.consent().ok_or((
                    400,
                    format!("`{name}` does not use OAuth2 with the user's consent (oauth2_grant = \"authorization_code\")"),
                ))?;
                let redirect = query
                    .get("redirect_uri")
                    .ok_or((400, "no redirect url".to_string()))?;
                let key = oauth::key().map_err(|e| (503, e))?;
                let e503 = |e: String| (503, e);
                let base = self.setting_value(&consent.authorize_url).map_err(e503)?;
                let client_id = self.setting_value(&consent.client_id).map_err(e503)?;
                let state = oauth::State::new(name, redirect, &format!("user:{}", who.user_id));
                let url = oauth::authorize_url(
                    &base,
                    &client_id,
                    consent.scope.as_deref(),
                    &state,
                    &state.seal(&key),
                );
                Ok(reply(302, json!({ "location": url })))
            }
            [name, "disconnect"] => {
                let n = name.to_string();
                let had = self
                    .side(move |c| quadra_store::grants::forget(c, &n))
                    .map_err(|e| (500, e))?;
                oauth::replace(&oauth::cache_key(&self.url, name), name, None);
                Ok(reply(
                    200,
                    json!({ "connection": name, "disconnected": had }),
                ))
            }
            _ => Err((404, "no such path".into())),
        }
    }
}

/// Unix seconds as the system tables write instants (UTC).
fn stamp(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%S").to_string())
        .unwrap_or_default()
}
