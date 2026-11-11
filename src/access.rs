use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http::{Method, StatusCode, header};

use crate::app::App;

pub fn local_hosts(port: u16) -> Vec<String> {
    vec![
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
        format!("localhost:{port}"),
    ]
}

fn deny(status: StatusCode, message: &str) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::json!({ "error": message }).to_string()))
        .expect("valid response")
}

pub fn is_proxy_path(path: &str) -> bool {
    path.starts_with("/v1/") || path.starts_with("/backend-api/")
}

fn is_machine_path(path: &str) -> bool {
    is_proxy_path(path) || path == "/conflux/control" || path == "/conflux/link"
}

fn changes_state(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

pub enum Verdict {
    Allow,
    Deny(StatusCode, &'static str),
}

pub fn check(allowed_hosts: &[String], method: &Method, path: &str, headers: &http::HeaderMap) -> Verdict {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !allowed_hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
        return Verdict::Deny(StatusCode::MISDIRECTED_REQUEST, "host not allowed");
    }
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if is_machine_path(path) {
        if origin.is_some() {
            return Verdict::Deny(StatusCode::FORBIDDEN, "browser requests are not allowed on this route");
        }
        return Verdict::Allow;
    }
    if changes_state(method) {
        if let Some(o) = origin {
            let same =
                o.eq_ignore_ascii_case(&format!("http://{host}")) || o.eq_ignore_ascii_case(&format!("https://{host}"));
            if !same {
                return Verdict::Deny(StatusCode::FORBIDDEN, "cross-origin request");
            }
        }
        let json = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.to_ascii_lowercase().starts_with("application/json"));
        if !json {
            return Verdict::Deny(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "content-type must be application/json",
            );
        }
    }
    Verdict::Allow
}

pub async fn guard(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let allowed = app.allowed_hosts.read().unwrap().clone();
    match check(&allowed, req.method(), req.uri().path(), req.headers()) {
        Verdict::Allow => next.run(req).await,
        Verdict::Deny(status, message) => deny(status, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    fn allowed() -> Vec<String> {
        local_hosts(8317)
    }

    #[test]
    fn state_changes_need_json_and_same_origin() {
        let form = headers(&[("host", "localhost:8317"), ("content-type", "text/plain")]);
        assert!(matches!(
            check(&allowed(), &Method::POST, "/conflux/api/x", &form),
            Verdict::Deny(..)
        ));
        let cross = headers(&[
            ("host", "localhost:8317"),
            ("content-type", "application/json"),
            ("origin", "http://evil.com"),
        ]);
        assert!(matches!(
            check(&allowed(), &Method::POST, "/conflux/api/x", &cross),
            Verdict::Deny(..)
        ));
        let same = headers(&[
            ("host", "localhost:8317"),
            ("content-type", "application/json"),
            ("origin", "http://localhost:8317"),
        ]);
        assert!(matches!(
            check(&allowed(), &Method::POST, "/conflux/api/x", &same),
            Verdict::Allow
        ));
        let cli = headers(&[("host", "127.0.0.1:8317"), ("content-type", "application/json")]);
        assert!(matches!(
            check(&allowed(), &Method::DELETE, "/conflux/api/x", &cli),
            Verdict::Allow
        ));
    }

    #[test]
    fn a_web_page_cannot_open_the_control_connection() {
        let page = headers(&[("host", "localhost:8317"), ("origin", "http://localhost:8317")]);
        assert!(matches!(
            check(&allowed(), &Method::GET, "/conflux/control", &page),
            Verdict::Deny(..)
        ));
        let link = headers(&[
            ("host", "localhost:8317"),
            ("origin", "http://localhost:8317"),
            ("content-type", "application/json"),
        ]);
        assert!(matches!(
            check(&allowed(), &Method::POST, "/conflux/link", &link),
            Verdict::Deny(..)
        ));
        let node = headers(&[("host", "localhost:8317")]);
        assert!(matches!(
            check(&allowed(), &Method::GET, "/conflux/control", &node),
            Verdict::Allow
        ));
    }
}
