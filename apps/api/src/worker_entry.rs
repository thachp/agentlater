//! Cloudflare glue: HTTP entry, the per-project `TaskLedger` Durable Object, its alarm,
//! and the queue consumer that makes delivery attempts. Logic lives in `ledger`, `delivery` and `routes`.

use crate::assertion;
use crate::db::{Db, DbError, Param};
use crate::delivery::{attempt, Outgoing, ReadPlan, ResponseBody, Transport};
use crate::ledger::{
    hook_secret_name, ApiError, Claimed, Delivery, Dispatch, HookDelivery, HookReply, Ledger,
    ResponseRead, SealedSecret,
};
use crate::routes::{Route, Verb};
use crate::sealing::{Keyring, SealingKey, NONCE_BYTES};
use futures_util::future::{join_all, select, Either};
use futures_util::StreamExt;
use runlater_core::limits::{ATTEMPT_TIMEOUT_MS, CAPTURED_HEADERS};
use runlater_core::{
    CronRequest, HookRequest, HookUpdate, Outcome, PollBody, ScheduleRequest, SecretRequest,
    Status, WakeRequest, WatchRequest,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use worker::js_sys;
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::*;

/// Set by the entry Worker on requests it forwards to a ledger; never taken from callers.
const PROJECT_HEADER: &str = "RunLater-Project";

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

fn verb(method: &Method) -> Option<Verb> {
    match method {
        Method::Get => Some(Verb::Get),
        Method::Post => Some(Verb::Post),
        _ => None,
    }
}

fn segments(url: &Url) -> Vec<String> {
    url.path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

/// The keys project secrets are sealed with: `SECRETS_KEY` (32 bytes as hex), and while it is
/// being rotated `SECRETS_KEY_PREVIOUS`. Secrets are unavailable without the first, and when
/// either is malformed: a mistyped previous key must be loud, not quietly fail old values.
fn sealing_key(env: &Env) -> Option<Keyring> {
    let read = |name: &str| {
        env.secret(name)
            .map(|s| s.to_string())
            .or_else(|_| env.var(name).map(|s| s.to_string()))
            .ok()
    };
    let current = SealingKey::parse(&read("SECRETS_KEY")?)?;
    let previous = match read("SECRETS_KEY_PREVIOUS") {
        Some(raw) => Some(SealingKey::parse(&raw)?),
        None => None,
    };
    Some(Keyring::new(current, previous))
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
    let route = verb(&method).and_then(|v| Route::parse(v, &path));
    let Some(scope) = route.and_then(Route::scope) else {
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

/// What a hook's sender is told: a status and an empty body, never anything it sent.
fn hook_answer(reply: HookReply) -> Result<Response> {
    let HookReply {
        status,
        reason,
        retry_after_ms,
    } = reply;
    let body = match reason {
        Some(r) => serde_json::json!({ "error": { "code": r } }),
        None => serde_json::json!({}),
    };
    let mut r = json(&body, status)?;
    if let Some(ms) = retry_after_ms {
        r.headers_mut()
            .set("Retry-After", &((ms + 999) / 1000).max(1).to_string())?;
    }
    Ok(r)
}

fn hooks_unavailable() -> Result<Response> {
    simple_error(
        503,
        "secrets_unavailable",
        "verified hooks are not configured on this deployment",
    )
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
    /// Seals `value` under `name` for this project with a fresh nonce; None when secrets
    /// aren't configured on this deployment.
    fn seal(&self, project_id: &str, name: &str, value: &str) -> Result<Option<String>> {
        let Some(key) = sealing_key(&self.env) else {
            return Ok(None);
        };
        let nonce: [u8; NONCE_BYTES] = random_bytes(NONCE_BYTES as u32)?
            .try_into()
            .map_err(|_| Error::RustError("short nonce".into()))?;
        Ok(Some(key.seal(project_id, name, value, nonce)))
    }

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

        let Some(route) = verb(&req.method()).and_then(|v| Route::parse(v, &path)) else {
            return simple_error(404, "not_found", "no such route");
        };
        let result = match route {
            Route::CreateProject => {
                let secret = format!("rls_{}", hex::encode(random_bytes(32)?));
                ledger
                    .create_project(&project_id, &secret, t)
                    .map(|(p, c)| created(&p, c))
            }
            Route::GetProject => ledger.project().map(|p| json(&p, 200)),
            Route::AddCredit => match body::<CreditBody>(req).await {
                Ok(c) => ledger
                    .add_credit(&c.payment_id, c.attempts, t)
                    .map(|(p, added)| {
                        if added {
                            record_stat(&self.env, &p.id, "payment", "real", c.attempts as f64);
                        }
                        created(&p, added)
                    }),
                Err(e) => Err(e),
            },
            Route::Schedule => match body::<ScheduleRequest>(req).await {
                Ok(r) => {
                    let id = format!("task_{}", hex::encode(random_bytes(10)?));
                    ledger
                        .schedule(r, &id, t)
                        .map(|(task, c)| created(&task, c))
                }
                Err(e) => Err(e),
            },
            Route::Watch => match body::<WatchRequest>(req).await {
                Ok(r) => {
                    let id = format!("task_{}", hex::encode(random_bytes(10)?));
                    ledger.watch(r, &id, t).map(|(task, c)| created(&task, c))
                }
                Err(e) => Err(e),
            },
            Route::ListTasks => match serde_urlencoded_query::<ListQuery>(&url) {
                Ok(q) => ledger
                    .list(q.status, q.lane.as_deref(), q.cursor.as_deref(), q.limit)
                    .map(|p| json(&p, 200)),
                Err(e) => Err(e),
            },
            Route::GetTask(id) => ledger.task(id).map(|v| json(&v, 200)),
            Route::CancelTask(id) => ledger.cancel(id, t).map(|v| json(&v, 200)),
            Route::RedriveTask(id) => ledger.redrive(id, t).map(|v| json(&v, 200)),
            Route::PutCron => match body::<CronRequest>(req).await {
                Ok(r) => ledger.put_cron(r, t).map(|(c, was)| created(&c, was)),
                Err(e) => Err(e),
            },
            Route::ListCrons => ledger.crons().map(|c| json(&c, 200)),
            Route::GetCron(name) => ledger.cron(name).map(|c| json(&c, 200)),
            Route::DeleteCron(name) => ledger.delete_cron(name).map(|c| json(&c, 200)),
            Route::ReadInbox => match body::<InboxReadBody>(req).await {
                Ok(r) => ledger
                    .read_inbox(r.after, r.limit, r.ack)
                    .map(|p| json(&p, 200)),
                Err(e) => Err(e),
            },
            Route::AckInbox => match body::<InboxAckBody>(req).await {
                Ok(a) => ledger.ack_inbox(a.up_to).map(|c| json(&c, 200)),
                Err(e) => Err(e),
            },
            Route::SetWake => match body::<WakeRequest>(req).await {
                Ok(w) => ledger.set_wake(w, t).map(|v| json(&v, 200)),
                Err(e) => Err(e),
            },
            Route::DeleteWake => ledger.delete_wake(t).map(|v| json(&v, 200)),
            Route::PutSecret => match body::<SecretRequest>(req).await {
                Ok(r) => match r.validate() {
                    Err(e) => Err(ApiError {
                        field: Some(e.field()),
                        ..ApiError::bad_request(e.to_string())
                    }),
                    Ok(spec) => {
                        let Some(sealed) = self.seal(&project_id, &spec.name, &spec.value)? else {
                            return simple_error(
                                503,
                                "secrets_unavailable",
                                "secrets are not configured on this deployment",
                            );
                        };
                        ledger
                            .put_secret(
                                &spec.name,
                                &sealed,
                                spec.value.len(),
                                &spec.allowed_hosts,
                                t,
                            )
                            .map(|(v, c)| created(&v, c))
                    }
                },
                Err(e) => Err(e),
            },
            Route::ListSecrets => ledger.secrets().map(|s| json(&s, 200)),
            Route::PutHook => match body::<HookRequest>(req).await {
                Ok(r) => match r.validate() {
                    Err(e) => Err(ApiError {
                        field: Some(e.field()),
                        ..ApiError::bad_request(e.to_string())
                    }),
                    Ok(spec) => {
                        let sealed = match &spec.verify {
                            None => None,
                            Some((_, secret)) => {
                                let name = hook_secret_name(&spec.name);
                                let Some(sealed) = self.seal(&project_id, &name, secret)? else {
                                    return hooks_unavailable();
                                };
                                Some(sealed)
                            }
                        };
                        let token = format!("hk_{}", hex::encode(random_bytes(32)?));
                        ledger
                            .put_hook(&spec, &token, sealed.as_deref(), t)
                            .map(|(h, c)| created(&h, c))
                    }
                },
                Err(e) => Err(e),
            },
            Route::ListHooks => ledger.hooks().map(|h| json(&h, 200)),
            Route::ReceiveHook => {
                let d = match body::<HookDelivery>(req).await {
                    Ok(d) => d,
                    Err(e) => return error(e),
                };
                let key = sealing_key(&self.env);
                let open = |name: &str, sealed: &str| key?.open(&project_id, name, sealed);
                return match ledger.take_hook_request(d, open, t) {
                    Ok(reply) => hook_answer(reply),
                    Err(e) => Err(Error::RustError(e.to_string())),
                };
            }
            Route::DeleteHook(name) => ledger.delete_hook(name).map(|v| json(&v, 200)),
            Route::UpdateHook(name) => match body::<HookUpdate>(req).await {
                Ok(u) => match u.validate() {
                    Err(e) => Err(ApiError {
                        field: Some(e.field()),
                        ..ApiError::bad_request(e.to_string())
                    }),
                    Ok(patch) => {
                        let sealed = match &patch.verify {
                            Some(Some((_, secret))) => {
                                let name = hook_secret_name(name);
                                let Some(sealed) = self.seal(&project_id, &name, secret)? else {
                                    return hooks_unavailable();
                                };
                                Some(sealed)
                            }
                            _ => None,
                        };
                        ledger
                            .update_hook(name, patch, sealed.as_deref())
                            .map(|v| json(&v, 200))
                    }
                },
                Err(e) => Err(e),
            },
            Route::DeleteSecret(name) => ledger.delete_secret(name).map(|v| json(&v, 200)),
            Route::ResealSecrets => {
                let Some(keys) = sealing_key(&self.env) else {
                    return simple_error(503, "secrets_unavailable", "secrets are not configured");
                };
                let count = ledger.reseal(|name, sealed| {
                    let nonce: [u8; NONCE_BYTES] = random_bytes(NONCE_BYTES as u32)
                        .ok()
                        .and_then(|b| b.try_into().ok())
                        .ok_or_else(|| DbError("secure randomness unavailable".into()))?;
                    Ok(keys.reseal(&project_id, name, sealed, nonce))
                });
                return match count {
                    Ok(c) => json(
                        &serde_json::json!({
                            "rotation": keys.rotation(),
                            // Which GitHub slot the deploy took the current key from, so
                            // scripts/rotate-secrets-key.mjs can tell the sweep saw these keys.
                            "slot": self.env.var("SECRETS_KEY_SLOT").ok().map(|v| v.to_string()),
                            "resealed": c.resealed,
                            "unreadable": c.unreadable,
                        }),
                        200,
                    ),
                    Err(e) => Err(Error::RustError(e.to_string())),
                };
            }
            Route::Claim => match body::<ClaimBody>(req).await {
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
            Route::Finish => match body::<FinishBody>(req).await {
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
            Ok(l) if sealing_key(&self.env).is_some() => l,
            Ok(l) => l.without_secrets(),
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
        for e in ledger.recover(t, jitter).map_err(failed)?.unreadable {
            console_error!("failed an unreadable task row: {e}");
        }
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

/// `double1` of an attempt whose lateness says nothing about AgentLater ([`Delivery::late_ms`]).
const NOT_MEASURED: f64 = -1.0;

/// Whether an attempt is real use or the monitor's journey, which delivers to our own receiver.
fn attempt_source(url: &str) -> &'static str {
    let probe = Url::parse(url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.starts_with("runlater-receiver-") && h.ends_with(".workers.dev"))
        })
        .unwrap_or(false);
    if probe {
        "probe"
    } else {
        "real"
    }
}

/// Adds a row to the numbers `scripts/stats.mjs` reports. Holds no URLs or contents. A failed
/// write loses one row of a statistic, so it never gets in the way of the work being counted.
fn record_stat(env: &Env, project_id: &str, event: &str, source: &str, value: f64) {
    let point = AnalyticsEngineDataPointBuilder::new()
        .indexes([project_id])
        .add_blob(event)
        .add_blob(source)
        .add_double(value)
        .build();
    if let Err(e) = env
        .analytics_engine("STATS")
        .and_then(|stats| stats.write_data_point(&point))
    {
        console_error!("recording a {event} stat failed: {e}");
    }
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
    record_stat(
        env,
        &d.project_id,
        "attempt",
        attempt_source(&delivery.url),
        delivery.late_ms.map_or(NOT_MEASURED, |ms| ms as f64),
    );
    // Secrets are opened only here, and only for as long as this attempt takes.
    let opened = open_secrets(env, &d.project_id, &delivery.secrets);
    if opened.is_none() {
        console_error!("could not open the secrets for {}", delivery.task_id);
    }
    let settled = attempt(&delivery, opened, now() / 1000, &WorkersFetch).await;
    let _: serde_json::Value = call(
        &stub,
        &d.project_id,
        "/internal/finish",
        &FinishBody {
            task_id: delivery.task_id,
            fence: delivery.fence,
            outcome: settled.outcome,
            body: settled.body,
            truncated: settled.truncated,
            headers: settled.headers,
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

/// Sends for real, with the Workers `fetch`.
struct WorkersFetch;

impl Transport for WorkersFetch {
    async fn send(&self, request: &Outgoing) -> (Outcome, Option<ResponseBody>) {
        send_with_fetch(request)
            .await
            .unwrap_or((Outcome::Network, None))
    }
}

async fn send_with_fetch(r: &Outgoing) -> Result<(Outcome, Option<ResponseBody>)> {
    let headers = Headers::new();
    for (k, v) in &r.headers {
        headers.set(k, v)?;
    }
    let method = match r.method {
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
    if let Some(body) = &r.body {
        init.with_body(Some(body.as_str().into()));
    }
    let request = Request::new_with_init(&r.url, &init)?;

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
        Ok::<_, Error>(match r.read {
            ReadPlan::Status => (outcome, None),
            ReadPlan::Capture { max_bytes } => {
                // The destination has answered, so a slow or broken body must not turn that
                // answer into a failure (and a retry). Read only while the attempt has time left.
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
                let read = read_capped(&mut response, max_bytes);
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
                (outcome, Some(body))
            }
            // A body cut off mid-read is a failed poll, not an empty answer.
            ReadPlan::Poll { max_bytes } => match read_capped(&mut response, max_bytes).await {
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
        })
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
