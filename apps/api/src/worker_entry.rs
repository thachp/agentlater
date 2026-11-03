//! Cloudflare glue: HTTP entry, the per-project `TaskLedger` Durable Object, its alarm,
//! and the queue consumer that makes delivery attempts. Logic lives in `ledger`.

use crate::assertion::{self, Scope};
use crate::db::{Db, DbError, Param};
use crate::ledger::{ApiError, Claimed, Delivery, Dispatch, Ledger, ResponseRead, SealedSecret};
use crate::sealing::{SealingKey, NONCE_BYTES};
use futures_util::future::{join_all, select, Either};
use futures_util::StreamExt;
use runlater_core::limits::{
    ATTEMPT_TIMEOUT_MS, CAPTURED_HEADERS, CAPTURE_BODY_BYTES, MAX_WATCH_RESPONSE_BYTES,
};
use runlater_core::{
    fill, redact, CronRequest, Outcome, PollBody, ScheduleRequest, SecretRequest, Status,
    WakeRequest, WatchRequest,
};
use runlater_egress::{sign, Destination};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use worker::js_sys;
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::*;

/// Set by the entry Worker on requests it forwards to a ledger; never taken from callers.
const PROJECT_HEADER: &str = "RunLater-Project";
const USER_AGENT: &str = "RunLater/1 (+https://runlater.io)";

fn now() -> i64 {
    Date::now().as_millis() as i64
}

fn random_bytes(n: u32) -> Result<Vec<u8>> {
    let crypto = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("crypto"))?;
    let get = js_sys::Reflect::get(&crypto, &JsValue::from_str("getRandomValues"))?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| Error::RustError("secure randomness unavailable".into()))?;
    let bytes = js_sys::Uint8Array::new_with_length(n);
    get.call1(&crypto, &bytes)?;
    Ok(bytes.to_vec())
}

/// Uniform in [0, 1); only used to spread retries, so a fallback is fine.
fn jitter() -> f64 {
    random_bytes(4)
        .ok()
        .and_then(|b| b.try_into().ok())
        .map_or(0.5, |b: [u8; 4]| {
            f64::from(u32::from_le_bytes(b)) / 4_294_967_296.0
        })
}

fn json<T: Serialize>(value: &T, status: u16) -> Result<Response> {
    let mut r = Response::from_json(value)?.with_status(status);
    r.headers_mut().set("Cache-Control", "no-store")?;
    Ok(r)
}

fn error(e: ApiError) -> Result<Response> {
    if e.status >= 500 {
        console_error!("{}: {}", e.code, e.message);
    }
    json(&e.body(), e.status)
}

fn simple_error(status: u16, code: &'static str, message: &str) -> Result<Response> {
    error(ApiError {
        status,
        code,
        message: message.into(),
        field: None,
    })
}

// ---------------------------------------------------------------------------------------
// Durable Object SQL as a `Db`

struct Sql(SqlStorage);

fn bindings(params: Vec<Param>) -> Vec<SqlStorageValue> {
    params
        .into_iter()
        .map(|p| match p {
            Param::Null => SqlStorageValue::Null,
            Param::Int(i) => SqlStorageValue::Integer(i),
            Param::Text(s) => SqlStorageValue::String(s),
        })
        .collect()
}

impl Db for Sql {
    fn exec(&self, sql: &str, params: Vec<Param>) -> std::result::Result<(), DbError> {
        self.0
            .exec(sql, bindings(params))
            .map(|_| ())
            .map_err(|e| DbError(e.to_string()))
    }

    fn query<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: Vec<Param>,
    ) -> std::result::Result<Vec<T>, DbError> {
        self.0
            .exec(sql, bindings(params))
            .and_then(|c| c.to_array())
            .map_err(|e| DbError(e.to_string()))
    }
}

// ---------------------------------------------------------------------------------------
// Entry Worker: authenticate the gateway, then hand the request to the project's ledger.

fn required_scope(method: &Method, path: &[&str]) -> Option<Scope> {
    Some(match (method, path) {
        (Method::Post, ["v1", "projects"]) => Scope::Project,
        (Method::Get, ["v1", "projects", "self"]) => Scope::Read,
        (Method::Post, ["v1", "projects", "self", "credits"]) => Scope::Billing,
        (Method::Post, ["v1", "tasks"]) => Scope::Schedule,
        (Method::Get, ["v1", "tasks"]) => Scope::Read,
        (Method::Get, ["v1", "tasks", _]) => Scope::Read,
        (Method::Post, ["v1", "tasks", _, "cancel"]) => Scope::Cancel,
        (Method::Post, ["v1", "tasks", _, "redrive"]) => Scope::Schedule,
        (Method::Post, ["v1", "watches"]) => Scope::Schedule,
        (Method::Post, ["v1", "crons"]) => Scope::Schedule,
        (Method::Get, ["v1", "crons"]) => Scope::Read,
        (Method::Get, ["v1", "crons", _]) => Scope::Read,
        (Method::Post, ["v1", "crons", _, "delete"]) => Scope::Cancel,
        // Acking is part of reading: it moves the project's one cursor, nothing else.
        (Method::Post, ["v1", "inbox", "read"]) => Scope::Read,
        (Method::Post, ["v1", "inbox", "ack"]) => Scope::Read,
        (Method::Post, ["v1", "wake"]) => Scope::Schedule,
        (Method::Post, ["v1", "wake", "delete"]) => Scope::Cancel,
        (Method::Post, ["v1", "secrets"]) => Scope::Secrets,
        // Names and hosts only; values are never returned.
        (Method::Get, ["v1", "secrets"]) => Scope::Read,
        (Method::Post, ["v1", "secrets", _, "delete"]) => Scope::Secrets,
        _ => return None,
    })
}

fn segments(url: &Url) -> Vec<String> {
    url.path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

/// The key project secrets are sealed with (`SECRETS_KEY`, 32 bytes as hex). Secrets are
/// unavailable without it.
fn sealing_key(env: &Env) -> Option<SealingKey> {
    let raw = env
        .secret("SECRETS_KEY")
        .map(|s| s.to_string())
        .or_else(|_| env.var("SECRETS_KEY").map(|s| s.to_string()))
        .ok()?;
    SealingKey::parse(&raw)
}

fn gateway_key(env: &Env) -> Option<assertion::VerifyingKey> {
    let raw = env
        .secret("GATEWAY_PUBLIC_KEY")
        .map(|s| s.to_string())
        .or_else(|_| env.var("GATEWAY_PUBLIC_KEY").map(|s| s.to_string()))
        .ok()?;
    assertion::parse_public_key(&raw)
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let url = req.url()?;
    let segs = segments(&url);
    let path: Vec<&str> = segs.iter().map(String::as_str).collect();
    let method = req.method();

    if method == Method::Get && path == ["health"] {
        return json(&crate::health_body(), 200);
    }
    let Some(scope) = required_scope(&method, &path) else {
        return simple_error(404, "not_found", "no such route");
    };
    let Some(key) = gateway_key(&env) else {
        return simple_error(500, "misconfigured", "gateway public key is not configured");
    };
    let Some(token) = req.headers().get(assertion::HEADER)? else {
        return simple_error(401, "unauthorized", "missing assertion");
    };
    let claims = match assertion::verify(&token, &key, now() / 1000) {
        Ok(c) => c,
        Err(_) => return simple_error(401, "unauthorized", "invalid assertion"),
    };
    if !claims.allows(scope) {
        return simple_error(403, "forbidden", "assertion lacks the required scope");
    }

    let headers = Headers::new();
    headers.set(PROJECT_HEADER, &claims.project_id)?;
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(method.clone()).with_headers(headers);
    if method == Method::Post {
        init.with_body(Some(req.bytes().await?.into()));
    }
    let forwarded = Request::new_with_init(url.as_str(), &init)?;
    env.durable_object("LEDGER")?
        .get_by_name(&claims.project_id)?
        .fetch_with_request(forwarded)
        .await
}

// ---------------------------------------------------------------------------------------
// TaskLedger Durable Object: one per project.

#[durable_object]
pub struct TaskLedger {
    state: State,
    env: Env,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreditBody {
    payment_id: String,
    attempts: i64,
}

#[derive(Deserialize)]
struct ClaimBody {
    task_id: String,
    generation: u64,
}

#[derive(Serialize, Deserialize)]
struct ClaimReply {
    deliver: Option<Delivery>,
    skipped: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct FinishBody {
    task_id: String,
    fence: u64,
    outcome: Outcome,
    /// A watch's poll: up to 64 KiB of the response body, and whether it was longer. A task
    /// with `capture`: up to 16 KiB.
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    truncated: bool,
    /// With `capture`: the response headers worth keeping.
    #[serde(default)]
    headers: Vec<(String, String)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxReadBody {
    #[serde(default)]
    after: Option<i64>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    ack: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxAckBody {
    up_to: i64,
}

#[derive(Deserialize)]
struct ListQuery {
    status: Option<Status>,
    lane: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

async fn body<T: DeserializeOwned>(req: &mut Request) -> std::result::Result<T, ApiError> {
    let bytes = req
        .bytes()
        .await
        .map_err(|_| ApiError::bad_request("could not read request body"))?;
    serde_json::from_slice(&bytes).map_err(|e| ApiError::bad_request(format!("invalid JSON: {e}")))
}

fn created<T: Serialize>(value: &T, was_created: bool) -> Result<Response> {
    json(value, if was_created { 201 } else { 200 })
}

impl TaskLedger {
    fn sql(&self) -> Sql {
        Sql(self.state.storage().sql())
    }

    /// Points the alarm at the next thing the ledger needs to do.
    async fn arm(&self, ledger: &Ledger<'_, Sql>) -> Result<()> {
        let storage = self.state.storage();
        match ledger
            .next_wake()
            .map_err(|e| Error::RustError(e.to_string()))?
        {
            Some(at) => {
                let delay = (at - now()).max(0);
                storage.set_alarm(Duration::from_millis(delay as u64)).await
            }
            None => storage.delete_alarm().await,
        }
    }

    async fn route(&self, req: &mut Request, ledger: &Ledger<'_, Sql>) -> Result<Response> {
        let Some(project_id) = req.headers().get(PROJECT_HEADER)? else {
            return simple_error(400, "invalid_request", "missing project");
        };
        let url = req.url()?;
        let segs = segments(&url);
        let path: Vec<&str> = segs.iter().map(String::as_str).collect();
        let t = now();

        let result = match (req.method(), path.as_slice()) {
            (Method::Post, ["v1", "projects"]) => {
                let secret = format!("rls_{}", hex::encode(random_bytes(32)?));
                ledger
                    .create_project(&project_id, &secret, t)
                    .map(|(p, c)| created(&p, c))
            }
            (Method::Get, ["v1", "projects", "self"]) => ledger.project().map(|p| json(&p, 200)),
            (Method::Post, ["v1", "projects", "self", "credits"]) => {
                match body::<CreditBody>(req).await {
                    Ok(c) => ledger
                        .add_credit(&c.payment_id, c.attempts, t)
                        .map(|(p, added)| created(&p, added)),
                    Err(e) => Err(e),
                }
            }
            (Method::Post, ["v1", "tasks"]) => match body::<ScheduleRequest>(req).await {
                Ok(r) => {
                    let id = format!("task_{}", hex::encode(random_bytes(10)?));
                    ledger
                        .schedule(r, &id, t)
                        .map(|(task, c)| created(&task, c))
                }
                Err(e) => Err(e),
            },
            (Method::Post, ["v1", "watches"]) => match body::<WatchRequest>(req).await {
                Ok(r) => {
                    let id = format!("task_{}", hex::encode(random_bytes(10)?));
                    ledger.watch(r, &id, t).map(|(task, c)| created(&task, c))
                }
                Err(e) => Err(e),
            },
            (Method::Get, ["v1", "tasks"]) => match serde_urlencoded_query::<ListQuery>(&url) {
                Ok(q) => ledger
                    .list(q.status, q.lane.as_deref(), q.cursor.as_deref(), q.limit)
                    .map(|p| json(&p, 200)),
                Err(e) => Err(e),
            },
            (Method::Get, ["v1", "tasks", id]) => ledger.task(id).map(|v| json(&v, 200)),
            (Method::Post, ["v1", "tasks", id, "cancel"]) => {
                ledger.cancel(id, t).map(|v| json(&v, 200))
            }
            (Method::Post, ["v1", "tasks", id, "redrive"]) => {
                ledger.redrive(id, t).map(|v| json(&v, 200))
            }
            (Method::Post, ["v1", "crons"]) => match body::<CronRequest>(req).await {
                Ok(r) => ledger.put_cron(r, t).map(|(c, was)| created(&c, was)),
                Err(e) => Err(e),
            },
            (Method::Get, ["v1", "crons"]) => ledger.crons().map(|c| json(&c, 200)),
            (Method::Get, ["v1", "crons", name]) => ledger.cron(name).map(|c| json(&c, 200)),
            (Method::Post, ["v1", "crons", name, "delete"]) => {
                ledger.delete_cron(name).map(|c| json(&c, 200))
            }
            (Method::Post, ["v1", "inbox", "read"]) => match body::<InboxReadBody>(req).await {
                Ok(r) => ledger
                    .read_inbox(r.after, r.limit, r.ack)
                    .map(|p| json(&p, 200)),
                Err(e) => Err(e),
            },
            (Method::Post, ["v1", "inbox", "ack"]) => match body::<InboxAckBody>(req).await {
                Ok(a) => ledger.ack_inbox(a.up_to).map(|c| json(&c, 200)),
                Err(e) => Err(e),
            },
            (Method::Post, ["v1", "wake"]) => match body::<WakeRequest>(req).await {
                Ok(w) => ledger.set_wake(w, t).map(|v| json(&v, 200)),
                Err(e) => Err(e),
            },
            (Method::Post, ["v1", "wake", "delete"]) => {
                ledger.delete_wake(t).map(|v| json(&v, 200))
            }
            (Method::Post, ["v1", "secrets"]) => match body::<SecretRequest>(req).await {
                Ok(r) => match (r.validate(), sealing_key(&self.env)) {
                    (Err(e), _) => Err(ApiError {
                        field: Some(e.field()),
                        ..ApiError::bad_request(e.to_string())
                    }),
                    (Ok(_), None) => {
                        return simple_error(
                            503,
                            "secrets_unavailable",
                            "secrets are not configured on this deployment",
                        )
                    }
                    (Ok(spec), Some(key)) => {
                        let nonce: [u8; NONCE_BYTES] = random_bytes(NONCE_BYTES as u32)?
                            .try_into()
                            .map_err(|_| Error::RustError("short nonce".into()))?;
                        let sealed = key.seal(&project_id, &spec.name, &spec.value, nonce);
                        ledger
                            .put_secret(&spec.name, &sealed, &spec.allowed_hosts, t)
                            .map(|(v, c)| created(&v, c))
                    }
                },
                Err(e) => Err(e),
            },
            (Method::Get, ["v1", "secrets"]) => ledger.secrets().map(|s| json(&s, 200)),
            (Method::Post, ["v1", "secrets", name, "delete"]) => {
                ledger.delete_secret(name).map(|v| json(&v, 200))
            }
            (Method::Post, ["internal", "claim"]) => match body::<ClaimBody>(req).await {
                Ok(c) => ledger.claim(&c.task_id, c.generation, t).map(|claimed| {
                    let reply = match claimed {
                        Claimed::Deliver(d) => ClaimReply {
                            deliver: Some(*d),
                            skipped: None,
                        },
                        Claimed::Skip(why) => ClaimReply {
                            deliver: None,
                            skipped: Some(why.into()),
                        },
                    };
                    json(&reply, 200)
                }),
                Err(e) => Err(e),
            },
            (Method::Post, ["internal", "finish"]) => match body::<FinishBody>(req).await {
                Ok(f) => ledger
                    .finish_response(
                        &f.task_id,
                        f.fence,
                        f.outcome,
                        Some(ResponseRead {
                            body: f.body.as_deref().map(|text| PollBody {
                                text,
                                truncated: f.truncated,
                            }),
                            headers: &f.headers,
                        }),
                        t,
                        jitter(),
                    )
                    .map(|s| json(&serde_json::json!({ "status": s }), 200))
                    .map_err(ApiError::from),
                Err(e) => Err(e),
            },
            _ => return simple_error(404, "not_found", "no such route"),
        };
        match result {
            Ok(response) => response,
            Err(e) => error(e),
        }
    }
}

/// Query strings here are flat `key=value` pairs; parse them through JSON so serde does
/// the typing (numbers and enums) for us.
fn serde_urlencoded_query<T: DeserializeOwned>(url: &Url) -> std::result::Result<T, ApiError> {
    let mut map = serde_json::Map::new();
    for (k, v) in url.query_pairs() {
        let value = match v.parse::<u64>() {
            Ok(n) if k == "limit" => serde_json::Value::from(n),
            _ => serde_json::Value::from(v.into_owned()),
        };
        map.insert(k.into_owned(), value);
    }
    serde_json::from_value(map.into())
        .map_err(|e| ApiError::bad_request(format!("invalid query: {e}")))
}

impl DurableObject for TaskLedger {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let sql = self.sql();
        let ledger = match Ledger::open(&sql) {
            Ok(l) => l,
            Err(e) => return error(e.into()),
        };
        let response = self.route(&mut req, &ledger).await?;
        self.arm(&ledger).await?;
        Ok(response)
    }

    async fn alarm(&self) -> Result<Response> {
        let sql = self.sql();
        let ledger = Ledger::open(&sql).map_err(|e| Error::RustError(e.to_string()))?;
        let t = now();
        let failed = |e: DbError| Error::RustError(e.to_string());
        ledger.recover(t, jitter).map_err(failed)?;
        ledger.purge(t).map_err(failed)?;
        ledger.fire_crons(t).map_err(failed)?;
        if let Ok(project) = ledger.project() {
            let due = ledger.publish_due(&project.id, t).map_err(failed)?;
            if !due.is_empty() {
                // Already marked published: a failed send is retried after REPUBLISH_MS.
                let queue = self.env.queue("READY")?;
                for dispatch in due {
                    if let Err(e) = queue.send(&dispatch).await {
                        console_error!("queue send failed for {}: {e}", dispatch.task_id);
                    }
                }
            }
        }
        self.arm(&ledger).await?;
        Response::ok("")
    }
}

// ---------------------------------------------------------------------------------------
// Queue consumer: claim, deliver, record.

async fn call<T: DeserializeOwned>(
    stub: &Stub,
    project_id: &str,
    path: &str,
    payload: &impl Serialize,
) -> Result<T> {
    let headers = Headers::new();
    headers.set(PROJECT_HEADER, project_id)?;
    headers.set("Content-Type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(serde_json::to_string(payload)?.into()));
    let req = Request::new_with_init(&format!("https://ledger{path}"), &init)?;
    let mut res = stub.fetch_with_request(req).await?;
    if res.status_code() != 200 {
        return Err(Error::RustError(format!(
            "ledger {path} returned {}",
            res.status_code()
        )));
    }
    res.json().await
}

async fn process(env: &Env, d: &Dispatch) -> Result<()> {
    if !assertion::valid_project_id(&d.project_id) {
        return Ok(());
    }
    let stub = env.durable_object("LEDGER")?.get_by_name(&d.project_id)?;
    let claim: ClaimReply = call(
        &stub,
        &d.project_id,
        "/internal/claim",
        &serde_json::json!({ "task_id": d.task_id, "generation": d.generation }),
    )
    .await?;
    let Some(delivery) = claim.deliver else {
        return Ok(());
    };
    // Secrets are opened only here, and only for as long as this attempt takes.
    let values = match open_secrets(env, &d.project_id, &delivery.secrets) {
        Some(values) => values,
        None => {
            console_error!("could not open the secrets for {}", delivery.task_id);
            vec![]
        }
    };
    let (outcome, read) = if values.len() < delivery.secrets.len() {
        (Outcome::Rejected, None)
    } else {
        match filled(&delivery, &values) {
            Some(ready) => deliver(&ready).await,
            None => (Outcome::Rejected, None),
        }
    };
    // A response that echoes a credential back must not carry it into the ledger.
    let (body, truncated, headers) = read.map_or((None, false, vec![]), |r| {
        (
            r.body.map(|b| redact(&b, &values)),
            r.truncated,
            r.headers
                .into_iter()
                .map(|(k, v)| (k, redact(&v, &values)))
                .collect(),
        )
    });
    let _: serde_json::Value = call(
        &stub,
        &d.project_id,
        "/internal/finish",
        &FinishBody {
            task_id: delivery.task_id,
            fence: delivery.fence,
            outcome,
            body,
            truncated,
            headers,
        },
    )
    .await?;
    Ok(())
}

/// The plaintext of each sealed secret, or None if any cannot be opened.
fn open_secrets(
    env: &Env,
    project_id: &str,
    sealed: &[SealedSecret],
) -> Option<Vec<(String, String)>> {
    if sealed.is_empty() {
        return Some(vec![]);
    }
    let key = sealing_key(env)?;
    sealed
        .iter()
        .map(|s| Some((s.name.clone(), key.open(project_id, &s.name, &s.sealed)?)))
        .collect()
}

/// The delivery with its header references filled in.
fn filled(d: &Delivery, values: &[(String, String)]) -> Option<Delivery> {
    let lookup = |name: &str| {
        values
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    };
    let headers = d
        .headers
        .iter()
        .map(|(k, v)| Some((k.clone(), fill(v, lookup)?)))
        .collect::<Option<Vec<_>>>()?;
    Some(Delivery {
        headers,
        secrets: vec![],
        ..d.clone()
    })
}

#[event(queue)]
async fn queue(batch: MessageBatch<Dispatch>, env: Env, _ctx: Context) -> Result<()> {
    let messages = batch.messages()?;
    let results = join_all(messages.iter().map(|m| process(&env, m.body()))).await;
    for (message, result) in messages.iter().zip(results) {
        match result {
            Ok(()) => message.ack(),
            Err(e) => {
                console_error!("dispatch {} failed: {e}", message.body().task_id);
                message.retry();
            }
        }
    }
    Ok(())
}

/// A response as read for a watch's poll or a task's capture.
struct ResponseBody {
    /// None when a captured body could not be read in time.
    body: Option<String>,
    /// Whether the body was cut short.
    truncated: bool,
    /// [`CAPTURED_HEADERS`], `content-length` and `content-encoding`, for capture.
    headers: Vec<(String, String)>,
}

type Read = Option<ResponseBody>;

/// Makes one attempt. For a watch's poll, also returns up to 64 KiB of the response body;
/// for a task with `capture`, up to 16 KiB and a few headers.
async fn deliver(d: &Delivery) -> (Outcome, Read) {
    // Re-check policy on every attempt; the provider flag `global_fetch_strictly_public`
    // separately refuses private addresses after DNS resolution.
    let Ok(destination) = Destination::parse(&d.url) else {
        return (Outcome::Rejected, None);
    };
    let body = d.body.clone().unwrap_or_default();
    let signature = sign(
        &[d.signing_secret.as_bytes()],
        &d.task_id,
        d.attempt,
        now() / 1000,
        body.as_bytes(),
    );
    match send(&destination, d, body, signature.headers()).await {
        Ok(result) => result,
        Err(_) => (Outcome::Network, None),
    }
}

/// Reads at most `max` bytes of a response, and whether there was more.
async fn read_capped(response: &mut Response, max: usize) -> Result<(String, bool)> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    // A response with no body has no stream; that is an empty body, not an error.
    if let Ok(mut stream) = response.stream() {
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk?);
            if bytes.len() > max {
                bytes.truncate(max);
                truncated = true;
                break;
            }
        }
    }
    Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}

/// Stop reading a captured body this long before the attempt times out, so the outcome is
/// recorded rather than lost to the timeout.
const CAPTURE_READ_MARGIN_MS: i64 = 500;

fn kept_headers(response: &Response) -> Vec<(String, String)> {
    CAPTURED_HEADERS
        .iter()
        .chain(&["content-length", "content-encoding"])
        .filter_map(|name| {
            let value = response.headers().get(name).ok().flatten()?;
            Some(((*name).to_string(), value))
        })
        .collect()
}

async fn send(
    destination: &Destination,
    d: &Delivery,
    body: String,
    signed: [(&'static str, String); 4],
) -> Result<(Outcome, Read)> {
    let headers = Headers::new();
    headers.set("User-Agent", USER_AGENT)?;
    for (k, v) in &d.headers {
        headers.set(k, v)?;
    }
    for (k, v) in &signed {
        headers.set(k, v)?;
    }
    let method = match d.method {
        runlater_core::Method::Get => Method::Get,
        runlater_core::Method::Post => Method::Post,
        runlater_core::Method::Put => Method::Put,
        runlater_core::Method::Patch => Method::Patch,
        runlater_core::Method::Delete => Method::Delete,
    };
    let mut init = RequestInit::new();
    init.with_method(method)
        .with_redirect(RequestRedirect::Manual)
        .with_headers(headers);
    if d.body.is_some() {
        init.with_body(Some(body.into()));
    }
    let request = Request::new_with_init(destination.as_str(), &init)?;

    let controller = AbortController::default();
    let signal = controller.signal();
    let fetch = Fetch::Request(request);
    let started = now();
    // The timeout covers reading a poll's body as well as the response itself.
    let attempt = async {
        let mut response = fetch.send_with_signal(&signal).await?;
        let outcome = Outcome::Http {
            status: response.status_code(),
            retry_after_ms: response
                .headers()
                .get("Retry-After")
                .ok()
                .flatten()
                .and_then(|v| crate::http::parse_retry_after(&v)),
        };
        if !d.read_body {
            return Ok::<_, Error>((outcome, None));
        }
        if d.capture {
            // The destination has answered, so a slow or broken body must not turn that answer
            // into a failure (and a retry). Read only while the attempt has time left.
            let headers = kept_headers(&response);
            let unread = |headers| ResponseBody {
                body: None,
                truncated: true,
                headers,
            };
            let left = started + ATTEMPT_TIMEOUT_MS - CAPTURE_READ_MARGIN_MS - now();
            if left <= 0 {
                return Ok((outcome, Some(unread(headers))));
            }
            let read = read_capped(&mut response, CAPTURE_BODY_BYTES);
            let limit = Delay::from(Duration::from_millis(left as u64));
            futures_util::pin_mut!(read, limit);
            let body = match select(read, limit).await {
                Either::Left((Ok((body, truncated)), _)) => ResponseBody {
                    body: Some(body),
                    truncated,
                    headers,
                },
                _ => unread(headers),
            };
            return Ok((outcome, Some(body)));
        }
        // A body cut off mid-read is a failed poll, not an empty answer.
        Ok(
            match read_capped(&mut response, MAX_WATCH_RESPONSE_BYTES).await {
                Ok((body, truncated)) => (
                    outcome,
                    Some(ResponseBody {
                        body: Some(body),
                        truncated,
                        headers: vec![],
                    }),
                ),
                Err(_) => (Outcome::Network, None),
            },
        )
    };
    let deadline = Delay::from(Duration::from_millis(ATTEMPT_TIMEOUT_MS as u64));
    futures_util::pin_mut!(attempt, deadline);
    let result = match select(attempt, deadline).await {
        Either::Left((Ok(result), _)) => result,
        Either::Left((Err(_), _)) => (Outcome::Network, None),
        Either::Right(_) => (Outcome::Timeout, None),
    };
    controller.abort();
    Ok(result)
}
