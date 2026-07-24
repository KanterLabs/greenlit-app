//! Authenticated GitHub configuration-variable lookup.
//!
//! Endpoints (verified against GitHub's current REST API reference while
//! implementing this module, `PHASE-3-actions.md` Variables: "Verify …
//! against GitHub's current documentation"):
//! - `GET /repos/{owner}/{repo}/actions/variables/{name}` — one repository
//!   variable (`name`, `value`, `created_at`, `updated_at`); 404 when the
//!   named variable does not exist.
//! - `GET /orgs/{org}/actions/variables/{name}` — one organization variable,
//!   same shape.
//! - `GET /repos/{owner}/{repo}/actions/variables` /
//!   `GET /orgs/{org}/actions/variables` — paginated (`per_page` up to 30,
//!   `page`) lists (`{"total_count": N, "variables": [...]}`), used only for
//!   a dynamic `vars[...]` lookup, which needs the complete applicable map
//!   rather than one name at a time.
//!
//! <https://docs.github.com/en/rest/actions/variables>
//!
//! Fine-grained permissions: the repository endpoints need the repository
//! "Variables" permission (read); the organization endpoints need the
//! organization-level "Variables" permission (read) — both documented on
//! the same reference page above and named in `crate::auth::pat`'s printed
//! PAT guidance. `litci auth`'s device-flow token is scoped by whatever the
//! installed `greenlit-app` GitHub App itself was granted; if that
//! installation lacks read access to Actions variables, every call here
//! reports [`RemoteVarError::Permission`] naming the exact permission to
//! grant instead of a bare HTTP status.
//!
//! GitHub's own ambiguity: like
//! `greenlit_actions::resolve::GitHubApiResolver` (see that type's
//! `ResolveError::NotFound` doc comment), a private/inaccessible repository
//! or organization and a real "no such variable" both surface from this API
//! as an ordinary 404 — this module cannot and does not try to distinguish
//! them, treating both as "absent" for a single-name lookup.

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

/// Real GitHub's REST API host.
pub(crate) const DEFAULT_API_BASE_URL: &str = "https://api.github.com";

const USER_AGENT: &str = concat!("greenlit-litci/", env!("CARGO_PKG_VERSION"));
const API_VERSION: &str = "2022-11-28";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// A single variable's JSON body is tiny; a list page is still small (at
/// most 30 variables), so this bounds a pathological/hostile response
/// rather than reflecting an expected size.
const MAX_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
/// GitHub's documented maximum `per_page` for both variables list
/// endpoints.
const MAX_PER_PAGE: u32 = 30;
/// A hard ceiling on pages fetched for one dynamic-lookup map, so a
/// pathological (or adversarially large) variable count cannot make
/// planning fetch unboundedly many pages before failing some other way.
const MAX_PAGES: u32 = 200;

/// Which scope-level a permission shortfall was reported against, named in
/// the fix action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionScope {
    /// The repository's own "Variables" (read) permission.
    Repository,
    /// The organization's "Variables" (read) permission.
    Organization,
}

impl PermissionScope {
    fn fix(self) -> &'static str {
        match self {
            PermissionScope::Repository => {
                "grant the installed GitHub App (or the fine-grained PAT) read-only 'Variables' repository permission, then run `litci auth` again"
            }
            PermissionScope::Organization => {
                "grant the installed GitHub App (or the fine-grained PAT) read-only organization-level 'Variables' permission, then run `litci auth` again"
            }
        }
    }
}

/// A remote variable lookup failed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RemoteVarError {
    /// The token lacks the permission the endpoint needs (HTTP 403).
    Permission {
        scope: PermissionScope,
        message: String,
    },
    /// Any other transport/API failure.
    Api(String),
}

impl RemoteVarError {
    /// Renders this error with its one fix action, for `crate::errors`.
    pub(crate) fn to_message_with_fix(&self) -> String {
        match self {
            RemoteVarError::Permission { scope, message } => {
                format!("{message}\n  fix: {}", scope.fix())
            }
            RemoteVarError::Api(message) => {
                format!(
                    "{message}\n  fix: check network connectivity and the token's validity, then retry"
                )
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct VariableResponse {
    // Present on every list-endpoint entry and read there
    // (`list_all`); a single-variable `get_one` response also carries this
    // field but that call site already knows the name it asked for, so it
    // only ever reads `value` from its own parsed `VariableResponse`.
    name: String,
    value: String,
}

#[derive(Debug, Deserialize)]
struct VariableListResponse {
    variables: Vec<VariableResponse>,
}

/// Talks to the GitHub configuration-variables REST endpoints. `crate::vars`
/// always constructs one via [`VariablesClient::with_base_url`] — production
/// passes real `https://api.github.com` (`DEFAULT_API_BASE_URL`, via
/// `crate::vars::api_base_url`, the same seam its internal test-only
/// override replaces); tests inject a loopback base URL directly. The same
/// pattern `greenlit_actions::resolve::GitHubApiResolver` uses (required
/// here for the identical reason: `PHASE-3-actions.md` exit criterion 5,
/// "variable endpoints use a mocked external GitHub endpoint").
#[derive(Debug, Clone)]
pub(crate) struct VariablesClient {
    agent: ureq::Agent,
    base_url: String,
    token: String,
}

impl VariablesClient {
    pub(crate) fn with_base_url(token: impl Into<String>, base_url: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .build();
        Self {
            agent: config.into(),
            base_url: base_url.into(),
            token: token.into(),
        }
    }

    /// Looks up one repository variable. `Ok(None)` means GitHub reported no
    /// such variable (or the repository itself is inaccessible — see the
    /// module doc comment).
    pub(crate) fn get_repo_variable(
        &self,
        owner: &str,
        repo: &str,
        name: &str,
    ) -> Result<Option<String>, RemoteVarError> {
        let url = format!(
            "{}/repos/{owner}/{repo}/actions/variables/{name}",
            self.base_url
        );
        self.get_one(&url, PermissionScope::Repository)
    }

    /// Looks up one organization variable. `Ok(None)` means GitHub reported
    /// no such variable, or `owner` does not name an organization at all
    /// (a personal-account `owner/repo` legitimately 404s here).
    pub(crate) fn get_org_variable(
        &self,
        org: &str,
        name: &str,
    ) -> Result<Option<String>, RemoteVarError> {
        let url = format!("{}/orgs/{org}/actions/variables/{name}", self.base_url);
        self.get_one(&url, PermissionScope::Organization)
    }

    fn get_one(&self, url: &str, scope: PermissionScope) -> Result<Option<String>, RemoteVarError> {
        let mut response = self
            .agent
            .get(url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", USER_AGENT)
            .header("X-GitHub-Api-Version", API_VERSION)
            .call()
            .map_err(|error| RemoteVarError::Api(format!("could not reach {url}: {error}")))?;
        let status = response.status().as_u16();
        let body = read_body(&mut response)?;
        if status == 404 {
            return Ok(None);
        }
        if status == 403 {
            return Err(RemoteVarError::Permission {
                scope,
                message: format!("HTTP 403: {}", String::from_utf8_lossy(&body).trim()),
            });
        }
        if status != 200 {
            return Err(RemoteVarError::Api(format!(
                "HTTP {status}: {}",
                String::from_utf8_lossy(&body).trim()
            )));
        }
        let parsed: VariableResponse = serde_json::from_slice(&body).map_err(|error| {
            RemoteVarError::Api(format!("unexpected variable response: {error}"))
        })?;
        Ok(Some(parsed.value))
    }

    /// Fetches the complete repository variable map (every page), for a
    /// dynamic `vars[...]` lookup.
    pub(crate) fn list_repo_variables(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<HashMap<String, String>, RemoteVarError> {
        let base = format!("{}/repos/{owner}/{repo}/actions/variables", self.base_url);
        self.list_all(&base, PermissionScope::Repository, true)
    }

    /// Fetches the complete organization variable map. An organization that
    /// does not exist for `org` (a personal-account owner) is treated as an
    /// empty map on its very first page, the same "absent" reasoning
    /// [`get_org_variable`](Self::get_org_variable) uses, rather than an
    /// error — most repositories are not org-owned, and this lookup runs
    /// unconditionally for every dynamic lookup regardless of ownership.
    pub(crate) fn list_org_variables(
        &self,
        org: &str,
    ) -> Result<HashMap<String, String>, RemoteVarError> {
        let base = format!("{}/orgs/{org}/actions/variables", self.base_url);
        self.list_all(&base, PermissionScope::Organization, false)
    }

    fn list_all(
        &self,
        base_url: &str,
        scope: PermissionScope,
        error_on_404: bool,
    ) -> Result<HashMap<String, String>, RemoteVarError> {
        let mut all = HashMap::new();
        for page in 1..=MAX_PAGES {
            let url = format!("{base_url}?per_page={MAX_PER_PAGE}&page={page}");
            let mut response = self
                .agent
                .get(&url)
                .header("Authorization", format!("Bearer {}", self.token))
                .header("Accept", "application/vnd.github+json")
                .header("User-Agent", USER_AGENT)
                .header("X-GitHub-Api-Version", API_VERSION)
                .call()
                .map_err(|error| RemoteVarError::Api(format!("could not reach {url}: {error}")))?;
            let status = response.status().as_u16();
            let body = read_body(&mut response)?;
            if status == 404 && !error_on_404 {
                return Ok(all);
            }
            if status == 403 {
                return Err(RemoteVarError::Permission {
                    scope,
                    message: format!("HTTP 403: {}", String::from_utf8_lossy(&body).trim()),
                });
            }
            if status != 200 {
                return Err(RemoteVarError::Api(format!(
                    "HTTP {status}: {}",
                    String::from_utf8_lossy(&body).trim()
                )));
            }
            let parsed: VariableListResponse = serde_json::from_slice(&body).map_err(|error| {
                RemoteVarError::Api(format!("unexpected variable list response: {error}"))
            })?;
            let page_len = parsed.variables.len();
            for variable in parsed.variables {
                all.insert(variable.name, variable.value);
            }
            if page_len < MAX_PER_PAGE as usize {
                break;
            }
        }
        Ok(all)
    }
}

fn read_body(response: &mut ureq::http::Response<ureq::Body>) -> Result<Vec<u8>, RemoteVarError> {
    response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .map_err(|error| RemoteVarError::Api(format!("could not read GitHub's response: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn drain_request(stream: &mut TcpStream) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        let mut buf = [0_u8; 8192];
        let mut total = Vec::new();
        loop {
            let read = stream.read(&mut buf).unwrap_or(0);
            if read == 0 {
                break;
            }
            total.extend_from_slice(&buf[..read]);
            if total.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
    }

    fn respond_once(listener: TcpListener, status: u16, reason: &'static str, body: String) {
        let (mut stream, _) = listener.accept().expect("accept");
        drain_request(&mut stream);
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    }

    #[test]
    fn resolves_a_repository_variable() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            respond_once(
                listener,
                200,
                "OK",
                r#"{"name":"MODE","value":"ci","created_at":"","updated_at":""}"#.to_string(),
            );
        });
        let client = VariablesClient::with_base_url("token", base_url);
        let value = client.get_repo_variable("owner", "repo", "MODE").unwrap();
        assert_eq!(value, Some("ci".to_string()));
        handle.join().unwrap();
    }

    #[test]
    fn a_404_resolves_to_none() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            respond_once(
                listener,
                404,
                "Not Found",
                r#"{"message":"Not Found"}"#.to_string(),
            );
        });
        let client = VariablesClient::with_base_url("token", base_url);
        let value = client
            .get_repo_variable("owner", "repo", "MISSING")
            .unwrap();
        assert_eq!(value, None);
        handle.join().unwrap();
    }

    #[test]
    fn a_403_is_a_permission_error_naming_the_repository_scope() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            respond_once(
                listener,
                403,
                "Forbidden",
                r#"{"message":"Resource not accessible"}"#.to_string(),
            );
        });
        let client = VariablesClient::with_base_url("token", base_url);
        let error = client
            .get_repo_variable("owner", "repo", "MODE")
            .unwrap_err();
        match error {
            RemoteVarError::Permission { scope, .. } => {
                assert_eq!(scope, PermissionScope::Repository)
            }
            other => panic!("expected Permission, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn lists_every_page_of_repository_variables() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut first_page_vars = Vec::new();
            for i in 0..MAX_PER_PAGE {
                first_page_vars.push(format!(r#"{{"name":"VAR{i}","value":"v{i}"}}"#));
            }
            let first_page = format!(
                r#"{{"total_count":{},"variables":[{}]}}"#,
                MAX_PER_PAGE + 1,
                first_page_vars.join(",")
            );
            let (mut stream, _) = listener.accept().expect("accept first page");
            drain_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{first_page}",
                first_page.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            drop(stream);

            let second_page = r#"{"total_count":31,"variables":[{"name":"LAST","value":"last"}]}"#;
            let (mut stream, _) = listener.accept().expect("accept second page");
            drain_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{second_page}",
                second_page.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let client = VariablesClient::with_base_url("token", base_url);
        let all = client.list_repo_variables("owner", "repo").unwrap();
        assert_eq!(all.len(), MAX_PER_PAGE as usize + 1);
        assert_eq!(all.get("LAST"), Some(&"last".to_string()));
        handle.join().unwrap();
    }

    #[test]
    fn org_lookup_treats_a_404_as_no_organization_rather_than_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            respond_once(
                listener,
                404,
                "Not Found",
                r#"{"message":"Not Found"}"#.to_string(),
            );
        });
        let client = VariablesClient::with_base_url("token", base_url);
        let all = client.list_org_variables("a-user-not-an-org").unwrap();
        assert!(all.is_empty());
        handle.join().unwrap();
    }
}
