//! The web interface: one page (`assets/index.html`) and a small JSON API.
//!
//! Sign-in: `!web` in Matrix sends a personal link `…/login#<token>`. The
//! page posts the token to `/api/login`, which answers with an HttpOnly
//! cookie; only the token's hash is stored. Identity is the Matrix user, so
//! the web and the room agree on who did what.
//!
//! Writes require the `X-HouseOps: 1` header on top of the SameSite cookie,
//! which a cross-site form or image request cannot send.

use std::collections::BTreeMap;

use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, patch, post},
    Json, Router,
};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tracing::info;

use crate::{
    house::{House, Origin},
    model::{Change, Priority, Status, TaskId},
    parse,
};

const INDEX: &str = include_str!("../assets/index.html");
const ICON: &str = include_str!("../assets/icon.svg");
const COOKIE: &str = "houseops";
const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;

pub async fn run(house: House, bind_addr: String) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&bind_addr).await?;
    info!("Web interface listening on {bind_addr}");
    axum::serve(listener, router(house)).await?;
    Ok(())
}

pub fn router(house: House) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/login", get(index))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icon.svg", get(icon))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/bootstrap", get(bootstrap))
        .route("/api/version", get(version))
        .route("/api/tasks", post(create_task))
        .route("/api/tasks/:id", patch(update_task))
        .route("/api/tasks/:id/comments", post(add_comment))
        .route(
            "/api/tasks/:id/attachments",
            post(upload_attachment).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route(
            "/api/tasks/:id/attachments/:attachment",
            delete(remove_attachment),
        )
        .route("/api/attachments/:attachment", get(download_attachment))
        .with_state(house)
}

// ── Errors and auth ───────────────────────────────────────────────────────────

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::NOT_FOUND, e.to_string())
    }
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name == COOKIE && !value.is_empty()).then(|| value.to_owned())
        })
}

async fn user(house: &House, headers: &HeaderMap) -> Result<String, ApiError> {
    let token = session_token(headers)
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "Not signed in".into()))?;
    house
        .store()
        .session_user(&token, chrono::Utc::now())
        .await
        .ok_or_else(|| {
            ApiError(
                StatusCode::UNAUTHORIZED,
                "Sign-in link no longer valid".into(),
            )
        })
}

/// Signed-in user for a write request.
async fn writer(house: &House, headers: &HeaderMap) -> Result<String, ApiError> {
    if headers.get("x-houseops").and_then(|v| v.to_str().ok()) != Some("1") {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "Missing X-HouseOps header".into(),
        ));
    }
    user(house, headers).await
}

fn cookie(house: &House, value: &str, max_age: i64) -> HeaderValue {
    let secure = house
        .public_url()
        .is_some_and(|u| u.starts_with("https://"));
    let raw = format!(
        "{COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{}",
        if secure { "; Secure" } else { "" }
    );
    HeaderValue::from_str(&raw).expect("cookie is ASCII")
}

// ── Pages ─────────────────────────────────────────────────────────────────────

async fn index() -> Response {
    (
        [
            (header::CACHE_CONTROL, "no-cache"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; img-src 'self' blob: data:; style-src 'self' 'unsafe-inline'; \
                 script-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'",
            ),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Html(INDEX),
    )
        .into_response()
}

async fn manifest(State(house): State<House>) -> Response {
    let body = json!({
        "name": format!("{} tasks", house.config().name),
        "short_name": house.config().name,
        "start_url": "/",
        "display": "standalone",
        "background_color": "#f5f3ef",
        "theme_color": "#2f6f5e",
        "icons": [{ "src": "/icon.svg", "sizes": "any", "type": "image/svg+xml" }],
    });
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        body.to_string(),
    )
        .into_response()
}

async fn icon() -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        ICON,
    )
        .into_response()
}

// ── Session ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LoginBody {
    token: String,
}

async fn login(
    State(house): State<House>,
    Json(body): Json<LoginBody>,
) -> Result<Response, ApiError> {
    let token = body.token.trim();
    if house
        .store()
        .session_user(token, chrono::Utc::now())
        .await
        .is_none()
    {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "This link is not valid (any more). Send !web to the bot for a new one.".into(),
        ));
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie(&house, token, 400 * 24 * 3600));
    Ok(response)
}

async fn logout(State(house): State<House>, headers: HeaderMap) -> Result<Response, ApiError> {
    writer(&house, &headers).await?;
    if let Some(token) = session_token(&headers) {
        house.store().end_session(&token).await;
    }
    let mut response = Json(json!({ "ok": true })).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie(&house, "", 0));
    Ok(response)
}

// ── Reads ─────────────────────────────────────────────────────────────────────

async fn bootstrap(
    State(house): State<House>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let me = user(&house, &headers).await?;
    let state = house.store().read().await;
    let mut people: BTreeMap<String, String> = state.members.clone();
    people
        .entry(me.clone())
        .or_insert_with(|| state.name_of(&me));
    for task in state.tasks.values() {
        for u in std::iter::once(&task.created_by)
            .chain(&task.assignee)
            .chain(task.history.iter().map(|h| &h.actor))
        {
            people.entry(u.clone()).or_insert_with(|| state.name_of(u));
        }
    }
    let mut members: Vec<Value> = people
        .iter()
        .map(|(id, name)| json!({ "id": id, "name": name, "member": state.members.contains_key(id) }))
        .collect();
    members.sort_by_key(|m| m["name"].as_str().unwrap_or_default().to_lowercase());
    Ok(Json(json!({
        "me": { "id": me, "name": state.name_of(&me) },
        "house": {
            "name": house.config().name,
            "locations": house.config().locations,
            "categories": house.config().categories,
            "room_id": house.room_id().as_str(),
        },
        "people": members,
        "version": state.version,
        "tasks": state.tasks.values().collect::<Vec<_>>(),
    })))
}

async fn version(State(house): State<House>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    user(&house, &headers).await?;
    let v = house.store().read().await.version;
    Ok(Json(json!({ "version": v })))
}

// ── Writes ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateBody {
    text: String,
}

/// Quick add: the same syntax as `!task` ("Fix tap #kitchen !high").
async fn create_task(
    State(house): State<House>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> Result<Json<Value>, ApiError> {
    let me = writer(&house, &headers).await?;
    let draft =
        parse::parse_draft(&body.text, house.config()).ok_or_else(|| bad("What needs doing?"))?;
    let task = house.create(draft, &me).await;
    Ok(Json(json!(task)))
}

fn opt_string(v: &Value, field: &str) -> Result<Option<String>, ApiError> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        Value::String(s) => Ok(Some(s.trim().to_owned())),
        _ => Err(bad(format!("{field} must be text or null"))),
    }
}

/// Translate a PATCH body into changes; only fields present are touched,
/// `null` clears.
fn changes_from(
    body: &serde_json::Map<String, Value>,
    house: &House,
) -> Result<Vec<Change>, ApiError> {
    let mut changes = Vec::new();
    for (key, v) in body {
        let change = match key.as_str() {
            "title" => Change::Title(opt_string(v, key)?.ok_or_else(|| bad("A title is needed"))?),
            "description" => Change::Description(opt_string(v, key)?.unwrap_or_default()),
            "status" => {
                let s = opt_string(v, key)?
                    .and_then(|s| Status::parse(&s))
                    .ok_or_else(|| bad("Unknown status"))?;
                let waiting_on = body
                    .get("waiting_on")
                    .map(|w| opt_string(w, "waiting_on"))
                    .transpose()?
                    .flatten();
                Change::Status(s, waiting_on)
            }
            "waiting_on" => continue, // read with status
            "priority" => Change::Priority(
                opt_string(v, key)?
                    .and_then(|p| Priority::parse(&p))
                    .ok_or_else(|| bad("Unknown priority"))?,
            ),
            "location" => Change::Location(opt_string(v, key)?),
            "category" => {
                let c = opt_string(v, key)?;
                if c.as_deref()
                    .is_some_and(|c| house.config().category(c).is_none())
                {
                    return Err(bad("Unknown category"));
                }
                Change::Category(c)
            }
            "assignee" => {
                let a = opt_string(v, key)?;
                if a.as_deref()
                    .is_some_and(|a| !a.starts_with('@') || !a.contains(':'))
                {
                    return Err(bad("assignee must be a Matrix user ID"));
                }
                Change::Assignee(a)
            }
            "due" => Change::Due(match opt_string(v, key)? {
                None => None,
                Some(d) => Some(
                    NaiveDate::parse_from_str(&d, "%Y-%m-%d")
                        .map_err(|_| bad("due must be YYYY-MM-DD"))?,
                ),
            }),
            other => return Err(bad(format!("Unknown field {other}"))),
        };
        changes.push(change);
    }
    Ok(changes)
}

async fn update_task(
    State(house): State<House>,
    headers: HeaderMap,
    Path(id): Path<TaskId>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, ApiError> {
    let me = writer(&house, &headers).await?;
    let changes = changes_from(&body, &house)?;
    let task = house.change(id, &me, changes, Origin::Web).await?;
    Ok(Json(json!(task)))
}

#[derive(Deserialize)]
struct CommentBody {
    text: String,
}

async fn add_comment(
    State(house): State<House>,
    headers: HeaderMap,
    Path(id): Path<TaskId>,
    Json(body): Json<CommentBody>,
) -> Result<Json<Value>, ApiError> {
    let me = writer(&house, &headers).await?;
    if body.text.trim().is_empty() {
        return Err(bad("Say something"));
    }
    let task = house
        .change(id, &me, vec![Change::Comment(body.text)], Origin::Web)
        .await?;
    Ok(Json(json!(task)))
}

async fn upload_attachment(
    State(house): State<House>,
    headers: HeaderMap,
    Path(id): Path<TaskId>,
    mut multipart: Multipart,
) -> Result<Json<Value>, ApiError> {
    let me = writer(&house, &headers).await?;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| bad(e.to_string()))?
    {
        if field.name() != Some("file") {
            continue;
        }
        let file_name = field.file_name().unwrap_or("photo.jpg").to_owned();
        let mime = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_owned();
        if !(mime.starts_with("image/") || mime == "application/pdf") {
            return Err(bad("Photos (or PDFs) only"));
        }
        let data = field.bytes().await.map_err(|e| bad(e.to_string()))?;
        let task = house
            .add_web_attachment(id, &me, &file_name, &mime, data.to_vec())
            .await?;
        return Ok(Json(json!(task)));
    }
    Err(bad("No file"))
}

async fn remove_attachment(
    State(house): State<House>,
    headers: HeaderMap,
    Path((id, attachment)): Path<(TaskId, String)>,
) -> Result<Json<Value>, ApiError> {
    let me = writer(&house, &headers).await?;
    let task = house
        .change(
            id,
            &me,
            vec![Change::RemoveAttachment(attachment)],
            Origin::Web,
        )
        .await?;
    Ok(Json(json!(task)))
}

async fn download_attachment(
    State(house): State<House>,
    headers: HeaderMap,
    Path(attachment): Path<String>,
) -> Result<Response, ApiError> {
    user(&house, &headers).await?;
    let (meta, data) = house
        .store()
        .attachment(&attachment)
        .await
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "No such file".into()))?;
    // Images inline; anything else downloads, so it can't run as a page.
    let disposition = if meta.mime.starts_with("image/") && meta.mime != "image/svg+xml" {
        "inline".to_owned()
    } else {
        format!(
            "attachment; filename=\"{}\"",
            meta.file_name.replace('"', "")
        )
    };
    Ok((
        [
            (header::CONTENT_TYPE, meta.mime),
            (header::CONTENT_DISPOSITION, disposition),
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable".to_owned(),
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
        ],
        data,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::store::Store;

    async fn house() -> (House, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let config = toml::from_str("room_id = \"!room:example.org\"").unwrap();
        (
            House::offline(store, config, Some("https://h.example".into())).await,
            dir,
        )
    }

    async fn call(
        house: &House,
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-houseops", "1");
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, format!("{COOKIE}={c}"));
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = router(house.clone()).oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, value)
    }

    #[tokio::test]
    async fn sign_in_with_a_link_token_then_work_with_tasks() {
        let (house, _dir) = house().await;
        let token = house
            .store()
            .new_session("@alice:example.org", chrono::Utc::now())
            .await;

        let (status, headers, _) = call(
            &house,
            "POST",
            "/api/login",
            None,
            Some(json!({ "token": token })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let set_cookie = headers[header::SET_COOKIE].to_str().unwrap();
        assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("Secure"));

        let (status, _, task) = call(
            &house,
            "POST",
            "/api/tasks",
            Some(&token),
            Some(json!({ "text": "Fix the tap #kitchen !urgent" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(task["title"], "Fix the tap");
        assert_eq!(task["priority"], "urgent");
        assert_eq!(task["location"], "Kitchen");

        let (status, _, task) = call(
            &house,
            "PATCH",
            "/api/tasks/1",
            Some(&token),
            Some(
                json!({ "assignee": "@alice:example.org", "due": "2026-10-02", "location": null }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{task}");
        assert_eq!(task["status"], "doing", "claiming starts it");
        assert_eq!(task["due"], "2026-10-02");
        assert_eq!(task["location"], Value::Null);

        let (_, _, task) = call(
            &house,
            "POST",
            "/api/tasks/1/comments",
            Some(&token),
            Some(json!({ "text": "Washer ordered" })),
        )
        .await;
        let last = task["history"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["kind"], "comment");
        assert_eq!(last["actor"], "@alice:example.org");

        let (_, _, boot) = call(&house, "GET", "/api/bootstrap", Some(&token), None).await;
        assert_eq!(boot["me"]["id"], "@alice:example.org");
        assert_eq!(boot["tasks"].as_array().unwrap().len(), 1);
        assert!(boot["version"].as_u64().unwrap() >= 3);
    }

    #[tokio::test]
    async fn unauthenticated_and_forged_requests_are_refused() {
        let (house, _dir) = house().await;
        let token = house
            .store()
            .new_session("@a:example.org", chrono::Utc::now())
            .await;

        let (status, _, _) = call(&house, "GET", "/api/bootstrap", None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = call(
            &house,
            "POST",
            "/api/login",
            None,
            Some(json!({ "token": "nope" })),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // A write without the custom header (what a cross-site form sends).
        let req = Request::builder()
            .method("POST")
            .uri("/api/tasks")
            .header(header::COOKIE, format!("{COOKIE}={token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({ "text": "x" }).to_string()))
            .unwrap();
        let res = router(house.clone()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        let (status, _, err) = call(
            &house,
            "PATCH",
            "/api/tasks/1",
            Some(&token),
            Some(json!({ "title": "x" })),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{err}");
    }

    #[tokio::test]
    async fn invalid_edits_explain_themselves() {
        let (house, _dir) = house().await;
        let token = house
            .store()
            .new_session("@a:example.org", chrono::Utc::now())
            .await;
        call(
            &house,
            "POST",
            "/api/tasks",
            Some(&token),
            Some(json!({ "text": "t" })),
        )
        .await;

        for body in [
            json!({ "status": "sleeping" }),
            json!({ "category": "nonsense" }),
            json!({ "due": "next tuesday" }),
            json!({ "assignee": "alice" }),
            json!({ "colour": "red" }),
        ] {
            let (status, _, err) = call(
                &house,
                "PATCH",
                "/api/tasks/1",
                Some(&token),
                Some(body.clone()),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            assert!(err["error"].is_string());
        }

        let (status, _, task) = call(
            &house,
            "PATCH",
            "/api/tasks/1",
            Some(&token),
            Some(json!({ "status": "waiting", "waiting_on": "landlord" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(task["waiting_on"], "landlord");
    }

    #[tokio::test]
    async fn the_page_and_icon_are_served_with_safe_headers() {
        let (house, _dir) = house().await;
        let res = router(house.clone())
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().contains_key(header::CONTENT_SECURITY_POLICY));
        let res = router(house)
            .oneshot(
                Request::builder()
                    .uri("/manifest.webmanifest")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
