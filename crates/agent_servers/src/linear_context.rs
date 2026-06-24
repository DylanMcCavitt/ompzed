//! Read-only Linear context bridge with a secure credential boundary
//! (AGE-646 / ZED-09).
//!
//! Fork-native per the extension capability audit (no keychain, panel, or
//! agent-thread access in the sandbox). There is no Linear CLI, so this talks
//! to `https://api.linear.app/graphql` directly over the host `HttpClient`.
//!
//! Credential boundary — the API key never reaches the renderer:
//!
//! * The key is resolved (env `LINEAR_API_KEY`, then the keychain) and used to
//!   build the `Authorization` header *entirely inside a background task* in
//!   [`load_linear_context`], which hands the UI only a [`LinearContext`] of
//!   mapped data. The key is never returned to, stored by, or rendered in any
//!   view.
//! * Egress is pinned to `api.linear.app`; the key is never logged.
//!
//! Read-only invariant: the only GraphQL document ever sent is the single
//! `query` built by [`build_linear_query`] — never a `mutation`.

use anyhow::Result;
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, SharedString, Task};
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest, StatusCode};
use std::sync::Arc;

/// Keychain record URL for the Linear personal API key.
pub const LINEAR_CREDENTIAL_URL: &str = "https://api.linear.app";
/// Linear GraphQL endpoint. Egress is pinned here.
pub const LINEAR_GRAPHQL_URL: &str = "https://api.linear.app/graphql";
/// Environment variable consulted before the keychain (dev / CI override).
pub const LINEAR_API_KEY_ENV: &str = "LINEAR_API_KEY";
/// Default number of teams / projects / issues to request.
pub const LINEAR_CONTEXT_DEFAULT_LIMIT: usize = 10;

/// A read-only snapshot of the viewer's Linear context.
///
/// When [`authenticated`](Self::authenticated) is false the snapshot is empty
/// and [`status`](Self::status) explains why (not connected, or an error). The
/// bridge never crashes on a missing or rejected key — it degrades to this.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinearContext {
    pub authenticated: bool,
    pub status: Option<SharedString>,
    pub viewer: Option<SharedString>,
    pub teams: Vec<LinearTeam>,
    pub projects: Vec<SharedString>,
    pub issues: Vec<LinearIssue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinearTeam {
    pub key: SharedString,
    pub name: SharedString,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinearIssue {
    pub identifier: SharedString,
    pub title: SharedString,
}

impl LinearContext {
    fn unauthenticated() -> Self {
        Self {
            authenticated: false,
            status: Some("Linear not connected".into()),
            ..Default::default()
        }
    }

    fn error(message: impl Into<SharedString>) -> Self {
        Self {
            authenticated: false,
            status: Some(message.into()),
            ..Default::default()
        }
    }

    /// One-line label for the panel header: the viewer plus counts when
    /// authenticated, otherwise the degraded status.
    pub fn summary_label(&self) -> SharedString {
        if !self.authenticated {
            return self
                .status
                .clone()
                .unwrap_or_else(|| "Linear not connected".into());
        }
        let mut parts: Vec<String> = Vec::new();
        if let Some(viewer) = &self.viewer {
            parts.push(viewer.to_string());
        }
        parts.push(count_label(self.teams.len(), "team"));
        parts.push(count_label(self.projects.len(), "project"));
        parts.push(count_label(self.issues.len(), "issue"));
        parts.join(" · ").into()
    }
}

fn count_label(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Build the single read-only GraphQL query. The only document this bridge ever
/// sends; asserted in tests to be a `query`, never a `mutation`.
pub fn build_linear_query(limit: usize) -> String {
    format!(
        "query {{ viewer {{ name }} \
         teams(first: {limit}) {{ nodes {{ key name }} }} \
         projects(first: {limit}) {{ nodes {{ name }} }} \
         issues(first: {limit}) {{ nodes {{ identifier title }} }} }}"
    )
}

fn request_body(limit: usize) -> String {
    serde_json::json!({ "query": build_linear_query(limit) }).to_string()
}

#[derive(serde::Deserialize)]
struct Envelope {
    data: Option<DataNode>,
}

#[derive(serde::Deserialize)]
struct DataNode {
    viewer: Option<Viewer>,
    teams: Option<Connection<TeamNode>>,
    projects: Option<Connection<ProjectNode>>,
    issues: Option<Connection<IssueNode>>,
}

#[derive(serde::Deserialize)]
struct Viewer {
    name: Option<String>,
}

#[derive(serde::Deserialize)]
struct Connection<T> {
    #[serde(default = "Vec::new")]
    nodes: Vec<T>,
}

#[derive(serde::Deserialize)]
struct TeamNode {
    key: String,
    name: String,
}

#[derive(serde::Deserialize)]
struct ProjectNode {
    name: String,
}

#[derive(serde::Deserialize)]
struct IssueNode {
    identifier: String,
    title: String,
}

/// Parse a GraphQL response body into an authenticated context. Returns `None`
/// when the body is not the expected `{ "data": { … } }` envelope (the caller
/// maps that to an unauthenticated/error state).
fn parse_linear_context(body: &str) -> Option<LinearContext> {
    let envelope: Envelope = serde_json::from_str(body).ok()?;
    let data = envelope.data?;
    let teams = data
        .teams
        .map(|connection| connection.nodes)
        .unwrap_or_default()
        .into_iter()
        .map(|team| LinearTeam {
            key: team.key.into(),
            name: team.name.into(),
        })
        .collect();
    let projects = data
        .projects
        .map(|connection| connection.nodes)
        .unwrap_or_default()
        .into_iter()
        .map(|project| SharedString::from(project.name))
        .collect();
    let issues = data
        .issues
        .map(|connection| connection.nodes)
        .unwrap_or_default()
        .into_iter()
        .map(|issue| LinearIssue {
            identifier: issue.identifier.into(),
            title: issue.title.into(),
        })
        .collect();
    Some(LinearContext {
        authenticated: true,
        status: None,
        viewer: data
            .viewer
            .and_then(|viewer| viewer.name)
            .map(SharedString::from),
        teams,
        projects,
        issues,
    })
}

/// Fetch read-only Linear context with an explicitly provided key. This is the
/// AFK-testable unit: the key is injected so the network and parse paths can be
/// exercised against a fake HTTP client (the test platform keychain always
/// reads empty).
///
/// Auth failures (`400`/`401`/`403`, or a GraphQL auth error) degrade to an
/// unauthenticated snapshot; other transport/parse failures degrade to an error
/// snapshot. Never panics.
pub async fn fetch_linear_context(
    http: Arc<dyn HttpClient>,
    key: Arc<str>,
    limit: usize,
) -> LinearContext {
    let request = match HttpRequest::builder()
        .method(Method::POST)
        .uri(LINEAR_GRAPHQL_URL)
        .header("Authorization", key.as_ref())
        .header("Content-Type", "application/json")
        .body(AsyncBody::from(request_body(limit)))
    {
        Ok(request) => request,
        Err(_) => return LinearContext::error("Could not build the Linear request"),
    };

    let mut response = match http.send(request).await {
        Ok(response) => response,
        Err(_) => return LinearContext::error("Could not reach Linear"),
    };

    let status = response.status();
    // Our query is fixed and valid, so a 4xx client error here is an auth
    // problem (Linear returns 400 for a missing/invalid key). Treat it as
    // unauthenticated rather than a hard error.
    if matches!(
        status,
        StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        return LinearContext::unauthenticated();
    }
    if !status.is_success() {
        return LinearContext::error(format!("Linear returned HTTP {}", status.as_u16()));
    }

    let mut body = String::new();
    if response.body_mut().read_to_string(&mut body).await.is_err() {
        return LinearContext::error("Could not read the Linear response");
    }

    match parse_linear_context(&body) {
        Some(context) => context,
        None if body.to_ascii_lowercase().contains("authenticat") => {
            LinearContext::unauthenticated()
        }
        None => LinearContext::error("Unexpected Linear response"),
    }
}

/// Resolve the Linear key (env first, then keychain) and fetch context, all
/// inside one background task. The returned [`Task`] yields only the mapped
/// [`LinearContext`] — the key never crosses back to the caller, so a renderer
/// awaiting this never observes the credential.
pub fn load_linear_context(
    cx: &App,
    http: Arc<dyn HttpClient>,
    limit: usize,
) -> Task<LinearContext> {
    let env_key = env_key();
    let read = cx.read_credentials(LINEAR_CREDENTIAL_URL);
    cx.background_spawn(async move {
        let key = match env_key {
            Some(key) => Some(key),
            None => match read.await {
                Ok(Some((_username, password))) => String::from_utf8(password)
                    .ok()
                    .filter(|key| !key.is_empty())
                    .map(|key| Arc::from(key.as_str())),
                _ => None,
            },
        };
        match key {
            Some(key) => fetch_linear_context(http, key, limit).await,
            None => LinearContext::unauthenticated(),
        }
    })
}

fn env_key() -> Option<Arc<str>> {
    std::env::var(LINEAR_API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .map(|key| Arc::from(key.as_str()))
}

/// Persist a key in the keychain (used by the panel's connect action). The
/// renderer hands the key straight to the platform keychain and never keeps it.
pub fn store_linear_api_key(cx: &App, key: &str) -> Task<Result<()>> {
    cx.write_credentials(LINEAR_CREDENTIAL_URL, "linear", key.as_bytes())
}

/// Remove the stored key from the keychain (panel's disconnect action).
pub fn clear_linear_api_key(cx: &App) -> Task<Result<()>> {
    cx.delete_credentials(LINEAR_CREDENTIAL_URL)
}

/// Seed the keychain from `LINEAR_API_KEY` when present, so the connection
/// persists across sessions. Returns `None` when the env var is unset.
pub fn connect_linear_from_env(cx: &App) -> Option<Task<Result<()>>> {
    let key = std::env::var(LINEAR_API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())?;
    Some(store_linear_api_key(cx, &key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{FakeHttpClient, Response};
    use std::sync::Mutex;
    const OK_BODY: &str = r#"{
        "data": {
            "viewer": { "name": "Ada Lovelace" },
            "teams": { "nodes": [{ "key": "ENG", "name": "Engineering" }] },
            "projects": { "nodes": [{ "name": "OMP Native Zed" }] },
            "issues": {
                "nodes": [
                    { "identifier": "AGE-646", "title": "Linear bridge" },
                    { "identifier": "AGE-649", "title": "GitHub bridge" }
                ]
            }
        }
    }"#;

    #[derive(Default)]
    struct CapturedRequest {
        uri: String,
        authorization: Option<String>,
        body: String,
    }

    /// A fake HTTP client that records the request and replies with `status` +
    /// `body`.
    fn client(
        status: u16,
        body: &'static str,
    ) -> (Arc<dyn HttpClient>, Arc<Mutex<CapturedRequest>>) {
        let captured = Arc::new(Mutex::new(CapturedRequest::default()));
        let sink = captured.clone();
        let http = FakeHttpClient::create(move |mut request| {
            let sink = sink.clone();
            async move {
                let authorization = request
                    .headers()
                    .get("Authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let uri = request.uri().to_string();
                let mut request_body = String::new();
                request
                    .body_mut()
                    .read_to_string(&mut request_body)
                    .await
                    .ok();
                *sink.lock().expect("capture lock") = CapturedRequest {
                    uri,
                    authorization,
                    body: request_body,
                };
                Ok(Response::builder()
                    .status(status)
                    .body(AsyncBody::from(body))?)
            }
        });
        (http, captured)
    }

    #[gpui::test]
    async fn fetch_maps_data_and_sets_auth_header(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (http, captured) = client(200, OK_BODY);
        let context = fetch_linear_context(
            http,
            Arc::from("lin_api_secret"),
            LINEAR_CONTEXT_DEFAULT_LIMIT,
        )
        .await;

        assert!(context.authenticated);
        assert_eq!(context.viewer.as_deref(), Some("Ada Lovelace"));
        assert_eq!(
            context.teams,
            vec![LinearTeam {
                key: "ENG".into(),
                name: "Engineering".into()
            }]
        );
        assert_eq!(context.projects, vec![SharedString::from("OMP Native Zed")]);
        assert_eq!(
            context.issues,
            vec![
                LinearIssue {
                    identifier: "AGE-646".into(),
                    title: "Linear bridge".into()
                },
                LinearIssue {
                    identifier: "AGE-649".into(),
                    title: "GitHub bridge".into()
                },
            ]
        );
        assert_eq!(
            context.summary_label().as_ref(),
            "Ada Lovelace · 1 team · 1 project · 2 issues"
        );

        // The key reached Linear via the Authorization header, and egress is
        // pinned to the GraphQL endpoint.
        let captured = captured.lock().expect("capture lock");
        assert_eq!(captured.authorization.as_deref(), Some("lin_api_secret"));
        assert_eq!(captured.uri, LINEAR_GRAPHQL_URL);
        // Read-only invariant on the wire: a query, never a mutation.
        assert!(captured.body.contains("query"));
        assert!(!captured.body.contains("mutation"));
    }

    #[gpui::test]
    async fn unauthorized_degrades_to_unauthenticated(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (http, _captured) = client(401, "unauthorized");
        let context =
            fetch_linear_context(http, Arc::from("bad-key"), LINEAR_CONTEXT_DEFAULT_LIMIT).await;

        assert!(!context.authenticated);
        assert!(context.viewer.is_none());
        assert!(context.teams.is_empty());
        assert_eq!(context.summary_label().as_ref(), "Linear not connected");
    }

    #[gpui::test]
    async fn graphql_auth_error_on_200_degrades_to_unauthenticated(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (http, _captured) = client(
            200,
            r#"{"errors":[{"message":"Authentication required, not authenticated"}]}"#,
        );
        let context =
            fetch_linear_context(http, Arc::from("bad-key"), LINEAR_CONTEXT_DEFAULT_LIMIT).await;
        assert!(!context.authenticated);
    }

    #[gpui::test]
    async fn server_error_degrades_to_error(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let (http, _captured) = client(500, "boom");
        let context =
            fetch_linear_context(http, Arc::from("key"), LINEAR_CONTEXT_DEFAULT_LIMIT).await;
        assert!(!context.authenticated);
        assert_eq!(context.summary_label().as_ref(), "Linear returned HTTP 500");
    }

    #[test]
    fn query_is_read_only() {
        let query = build_linear_query(5);
        assert!(query.contains("query"));
        assert!(query.contains("viewer"));
        assert!(query.contains("first: 5"));
        assert!(!query.contains("mutation"));
    }

    #[test]
    fn summary_label_unauthenticated_uses_status() {
        assert_eq!(
            LinearContext::unauthenticated().summary_label().as_ref(),
            "Linear not connected"
        );
        assert_eq!(
            LinearContext::error("nope").summary_label().as_ref(),
            "nope"
        );
    }
}
