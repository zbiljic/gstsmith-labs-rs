//! An optional shared token for the controller.
//!
//! With a token, the control page, its API, and the calibration files,
//! photo, and previews answer only requests that carry it: in the cookie
//! the sign-in page sets, or as `Authorization: Bearer TOKEN` from scripts.
//! Display pages and the few requests they make stay open, so displays
//! need no setup; see [`display_route`]. Over plain HTTP, the token and the
//! cookie cross the network unencrypted; see the README.

use std::fmt;

use anyhow::{Result, bail};

use crate::http::{Request, Response};

/// The cookie that carries the token after signing in.
pub const COOKIE: &str = "wallplay_control";

/// Shortest token accepted, so that guessing one is hopeless.
pub const MIN_LENGTH: usize = 16;

const SIGN_IN_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>wallplay: sign in</title>
<style>
  :root { color-scheme: dark; }
  body { margin: 0 auto; max-width: 420px; padding: 32px 16px; font: 14px/1.45 system-ui, sans-serif; background: #161616; color: #eee; }
  form { display: flex; flex-direction: column; gap: 10px; }
  input, button { font: inherit; padding: 6px 10px; }
  .error { color: #ff6b6b; }
  .muted { color: #999; }
</style>
</head>
<body>
<h1>wallplay</h1>
<p>Enter the control token to use the control page.</p>
<p class="error">{{MESSAGE}}</p>
<form method="post" action="/login">
  <input type="password" name="token" autocomplete="current-password" required autofocus aria-label="Control token">
  <button>Sign in</button>
</form>
<p class="muted">Display pages need no token: open /display/ID on every display.</p>
</body>
</html>
"#;

/// The controller's token. Its `Debug` output hides it.
#[derive(Clone, PartialEq, Eq)]
pub struct ControlToken(String);

impl fmt::Debug for ControlToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ControlToken(..)")
    }
}

impl ControlToken {
    /// A token of at least [`MIN_LENGTH`] characters from letters, digits,
    /// and `-._~`, as hex and base64url tools write them, so that it fits
    /// unquoted in a cookie and a header, and unescaped in a URL. Surrounding
    /// whitespace, as a file's final newline, is ignored.
    pub fn new(token: &str) -> Result<Self> {
        let token = token.trim();
        if token.len() < MIN_LENGTH {
            bail!("the control token must have at least {MIN_LENGTH} characters");
        }
        if !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
        {
            bail!("the control token may hold only letters, digits, and -._~");
        }
        Ok(Self(token.to_owned()))
    }

    /// Whether `candidate` is the token, comparing in time independent of
    /// where they differ.
    #[must_use]
    pub fn matches(&self, candidate: &str) -> bool {
        let (expected, candidate) = (self.0.as_bytes(), candidate.as_bytes());
        expected.len() == candidate.len()
            && expected
                .iter()
                .zip(candidate)
                .fold(0, |difference, (left, right)| difference | (left ^ right))
                == 0
    }

    /// Whether `request` carries the token as a bearer token or in the
    /// sign-in cookie.
    #[must_use]
    pub fn authorizes(&self, request: &Request) -> bool {
        let headers = |name: &'static str| {
            request
                .headers
                .iter()
                .filter(move |(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        let bearer = headers("authorization").filter_map(|value| {
            let (scheme, token) = value.split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("bearer")
                .then_some(token.trim())
        });
        let cookies = headers("cookie")
            .flat_map(|value| value.split(';'))
            .filter_map(|cookie| cookie.trim().split_once('='))
            .filter(|(name, _)| *name == COOKIE)
            .map(|(_, value)| value);
        bearer.chain(cookies).any(|token| self.matches(token))
    }

    /// Answer a sign-in attempt with `candidate`: the control page, with
    /// the cookie set, or the sign-in page again.
    #[must_use]
    pub fn sign_in(&self, candidate: &str) -> Response {
        if !self.matches(candidate.trim()) {
            return sign_in_page("Wrong token.");
        }
        // A session cookie the page's scripts cannot read, sent only by
        // pages of this site, which keeps other sites from making requests
        // with it.
        let cookie = format!("{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict", self.0);
        Response::new(303, "text/plain", "")
            .with_header("Location", "/")
            .with_header("Set-Cookie", &cookie)
    }
}

/// The sign-in page, with `message` above the form.
#[must_use]
pub fn sign_in_page(message: &str) -> Response {
    // The messages are this module's own, but escape them anyway.
    let message = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let mut response = Response::html(SIGN_IN_PAGE.replace("{{MESSAGE}}", &message));
    response.status = 401;
    response.with_header("WWW-Authenticate", r#"Bearer realm="wallplay""#)
}

/// The answer to a request without the token for anything but the
/// control page.
#[must_use]
pub fn unauthorized() -> Response {
    Response::error(
        401,
        "this needs the control token: sign in on the control page, or send Authorization: Bearer TOKEN",
    )
    .with_header("WWW-Authenticate", r#"Bearer realm="wallplay""#)
}

/// Whether a route serves display pages, which stay open with a control
/// token: the page, its long poll, its pattern, and the clock and frame
/// reports it sends while measuring. Every other route, including routes
/// added later, needs the token.
#[must_use]
pub fn display_route(method: &str, segments: &[&str]) -> bool {
    matches!(
        (method, segments),
        (
            "GET",
            ["display"] | ["display", _] | ["api", "displays", _, "poll" | "pattern.svg"]
        ) | ("POST", ["api", "clock"] | ["api", "displays", _, "frames"])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123";

    fn token() -> ControlToken {
        ControlToken::new(TOKEN).expect("token")
    }

    fn with_header(name: &str, value: &str) -> Request {
        Request {
            headers: vec![(name.to_owned(), value.to_owned())],
            ..Request::default()
        }
    }

    #[test]
    fn tokens_must_be_long_and_plain() {
        for invalid in [
            "short",
            "0123456789abcdef; Path=/",
            "0123456789abcdef\u{e9}",
            // Base64 with `+`, `/` and `=`, which a link would need escaped.
            "0123456789+abcdef/xyz==",
        ] {
            assert!(ControlToken::new(invalid).ok().is_none(), "{invalid:?}");
        }
        // A file's final newline is not part of the token.
        assert_eq!(
            ControlToken::new(&format!("{TOKEN}\n")).expect("token"),
            token()
        );
        assert!(!format!("{:?}", token()).contains(TOKEN));
    }

    #[test]
    fn requests_carry_the_token_as_bearer_or_cookie() {
        let token = token();
        assert!(!token.authorizes(&Request::default()));
        assert!(token.authorizes(&with_header("Authorization", &format!("Bearer {TOKEN}"))));
        assert!(token.authorizes(&with_header("authorization", &format!("bearer  {TOKEN}"))));
        assert!(!token.authorizes(&with_header("Authorization", &format!("Basic {TOKEN}"))));
        assert!(!token.authorizes(&with_header("Authorization", "Bearer 0123456789abcdef0124")));
        assert!(!token.authorizes(&with_header("Authorization", &format!("Bearer {TOKEN}0"))));
        assert!(token.authorizes(&with_header(
            "Cookie",
            &format!("theme=dark; {COOKIE}={TOKEN}; other=1")
        )));
        assert!(!token.authorizes(&with_header("Cookie", &format!("x{COOKIE}={TOKEN}"))));
        assert!(!token.authorizes(&with_header("Cookie", &format!("{COOKIE}=nope"))));
        assert!(!token.authorizes(&with_header("Cookie", &format!("{COOKIE}=\"{TOKEN}\""))));
        // Every header of a name counts, as when a client sends several.
        let request = Request {
            headers: vec![
                ("Cookie".to_owned(), "theme=dark".to_owned()),
                ("Cookie".to_owned(), format!("{COOKIE}={TOKEN}")),
            ],
            ..Request::default()
        };
        assert!(token.authorizes(&request));
    }

    #[test]
    fn signing_in_sets_a_strict_http_only_cookie() {
        let token = token();
        let response = token.sign_in(TOKEN);
        assert_eq!(response.status, 303);
        let header = |name: &str| {
            response
                .headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(header("Location").as_deref(), Some("/"));
        let cookie = header("Set-Cookie").expect("cookie");
        assert!(
            cookie.starts_with(&format!("{COOKIE}={TOKEN};")),
            "{cookie}"
        );
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"));
        let wrong = token.sign_in("0123456789abcdef0124");
        assert_eq!(wrong.status, 401);
        assert!(wrong.headers.iter().all(|(key, _)| key != "Set-Cookie"));
        assert!(String::from_utf8_lossy(&wrong.body).contains("Wrong token."));
    }

    #[test]
    fn only_display_routes_stay_open() {
        for (method, path) in [
            ("GET", "display"),
            ("GET", "display/lobby"),
            ("GET", "api/displays/lobby/poll"),
            ("GET", "api/displays/lobby/pattern.svg"),
            ("POST", "api/clock"),
            ("POST", "api/displays/lobby/frames"),
        ] {
            let segments: Vec<&str> = path.split('/').collect();
            assert!(display_route(method, &segments), "{method} {path}");
        }
        for (method, path) in [
            ("GET", ""),
            ("POST", "display/lobby"),
            ("GET", "calibration"),
            ("GET", "api/state"),
            ("GET", "api/displays/lobby/preview.jpg"),
            ("DELETE", "api/displays/lobby"),
            ("GET", "api/clock"),
            ("GET", "api/displays/lobby/frames"),
            ("POST", "api/displays/lobby/poll"),
            ("GET", "api/photo.jpg"),
            ("POST", "api/source"),
            // The path is matched as sent: other methods, empty or
            // percent-encoded segments, and absolute targets match nothing.
            ("HEAD", "display/lobby"),
            ("get", "display/lobby"),
            ("GET", "api//displays/lobby/poll"),
            ("GET", "api%2Fdisplays/lobby/poll"),
            ("POST", "api/displays/lobby/frames/../../../play"),
            ("GET", "http:/host/display/lobby"),
        ] {
            let segments: Vec<&str> = if path.is_empty() {
                Vec::new()
            } else {
                path.split('/').collect()
            };
            assert!(!display_route(method, &segments), "{method} {path}");
        }
    }
}
