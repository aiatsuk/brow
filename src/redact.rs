//! Secret redaction, applied at ingest.
//!
//! Anything captured from a page can end up in an agent's context, in a log file
//! and in a bug report. Redacting at the point of capture — rather than at the
//! point of display — means a secret matched by these rules is never written to
//! disk in the first place,
//! so there is no later code path that can accidentally leak it.
//!
//! The tradeoff is real and worth stating: over-redaction destroys debuggability.
//! The rules here are deliberately narrow — named credential headers, named
//! credential query parameters, and token shapes that are unambiguous — rather
//! than an entropy heuristic that would eat request ids and content hashes.

/// What replaces a redacted value. Fixed-width on purpose: the length of a secret
/// is itself information.
pub const MASK: &str = "[redacted]";

/// Headers whose value is a credential by definition.
const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "x-auth-token",
    "x-authorization",
    "x-csrf-token",
    "x-xsrf-token",
    "x-session-token",
    "x-amz-security-token",
    "x-goog-api-key",
];

/// Query parameters that carry credentials in the wild.
const SECRET_PARAMS: &[&str] = &[
    "access_token",
    "api_key",
    "apikey",
    "auth",
    "auth_token",
    "code",
    "id_token",
    "key",
    "password",
    "passwd",
    "pwd",
    "refresh_token",
    "secret",
    "session",
    "sig",
    "signature",
    "token",
];

/// True when this header's value must never be recorded.
pub fn is_secret_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_HEADERS.contains(&lower.as_str())
}

/// Redacts a header value if the header is a credential carrier.
pub fn header(name: &str, value: &str) -> String {
    if is_secret_header(name) {
        MASK.to_string()
    } else {
        text(value)
    }
}

/// Rewrites credential-bearing query parameters out of a URL.
///
/// Operates on the raw string rather than a URL parser: we have no URL crate, the
/// grammar we care about is trivial, and a malformed URL should still get
/// redacted rather than passed through untouched.
pub fn url(raw: &str) -> String {
    // Fragments can carry tokens too (the OAuth implicit flow put them there).
    let (before_frag, frag) = match raw.split_once('#') {
        Some((a, b)) => (a, Some(b)),
        None => (raw, None),
    };

    let mut out = match before_frag.split_once('?') {
        None => redact_url_userinfo(before_frag),
        Some((base, query)) => {
            let redacted: Vec<String> = query
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((k, _)) if is_secret_param(k) => format!("{k}={MASK}"),
                    _ => pair.to_string(),
                })
                .collect();
            format!("{}?{}", redact_url_userinfo(base), redacted.join("&"))
        }
    };

    if let Some(frag) = frag {
        // A fragment that looks like a query string gets the same treatment.
        let redacted = if frag.contains('=') {
            frag.split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((k, _)) if is_secret_param(k) => format!("{k}={MASK}"),
                    _ => pair.to_string(),
                })
                .collect::<Vec<_>>()
                .join("&")
        } else {
            frag.to_string()
        };
        out.push('#');
        out.push_str(&redacted);
    }
    out
}

/// Masks the RFC 3986 `userinfo@` authority component. Passwords in URL
/// userinfo are credentials even when no query parameter carries a recognizable
/// name. The username is masked too because it can itself be sensitive.
fn redact_url_userinfo(base: &str) -> String {
    let Some(scheme) = base.find("://") else {
        return base.to_string();
    };
    let authority_start = scheme + 3;
    let authority_end = base[authority_start..]
        .find('/')
        .map(|offset| authority_start + offset)
        .unwrap_or(base.len());
    let authority = &base[authority_start..authority_end];
    let Some(at) = authority.rfind('@') else {
        return base.to_string();
    };
    format!(
        "{}{}@{}",
        &base[..authority_start],
        MASK,
        &base[authority_start + at + 1..]
    )
}

fn is_secret_param(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    SECRET_PARAMS.contains(&lower.as_str())
}

/// Masks unambiguous token shapes inside free text (console output, error
/// messages, response snippets).
///
/// Only two patterns, both of which have essentially no false-positive rate:
/// a `Bearer <token>` prefix, and a JWT's `eyJ...` header.
pub fn text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    loop {
        let bearer = find_ci(rest, "bearer ");
        let jwt = rest.find("eyJ");

        let hit = match (bearer, jwt) {
            (None, None) => break,
            (Some(b), None) => (b, Token::Bearer),
            (None, Some(j)) => (j, Token::Jwt),
            (Some(b), Some(j)) => {
                if b <= j {
                    (b, Token::Bearer)
                } else {
                    (j, Token::Jwt)
                }
            }
        };

        let (at, kind) = hit;
        match kind {
            Token::Bearer => {
                let value_start = at + "bearer ".len();
                let value = &rest[value_start..];
                let end = value
                    .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                    .unwrap_or(value.len());
                let candidate = &value[..end];

                // "the bearer of bad news" must survive. A real bearer token is at
                // least eight characters of RFC 6750 token charset; English words
                // that happen to follow the word "bearer" are neither.
                let looks_like_a_token = candidate.len() >= 8
                    && candidate
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-._~+/=".contains(c));

                out.push_str(&rest[..value_start]);
                if looks_like_a_token {
                    out.push_str(MASK);
                    rest = &value[end..];
                } else {
                    rest = value;
                }
            }
            Token::Jwt => {
                let value = &rest[at..];
                let end = value
                    .find(|c: char| {
                        !(c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
                    })
                    .unwrap_or(value.len());
                let candidate = &value[..end];
                // A JWT is three dot-separated segments. `eyJ` at the start of an
                // ordinary word is not one.
                if candidate.matches('.').count() >= 2 && candidate.len() > 32 {
                    out.push_str(&rest[..at]);
                    out.push_str(MASK);
                    rest = &value[end..];
                } else {
                    let advance = at + 3;
                    out.push_str(&rest[..advance]);
                    rest = &rest[advance..];
                }
            }
        }
    }

    out.push_str(rest);
    out
}

enum Token {
    Bearer,
    Jwt,
}

/// Case-insensitive substring search for an ASCII needle.
fn find_ci(haystack: &str, needle_lower: &str) -> Option<usize> {
    let n = needle_lower.len();
    if haystack.len() < n {
        return None;
    }
    haystack
        .as_bytes()
        .windows(n)
        .position(|w| w.eq_ignore_ascii_case(needle_lower.as_bytes()))
        .filter(|&i| haystack.is_char_boundary(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_headers_are_masked_by_name() {
        assert_eq!(header("Authorization", "Bearer abc123"), MASK);
        assert_eq!(header("COOKIE", "session=xyz"), MASK);
        assert_eq!(header("set-cookie", "a=b; HttpOnly"), MASK);
        assert_eq!(header("X-API-Key", "k-123"), MASK);
    }

    #[test]
    fn ordinary_headers_survive() {
        assert_eq!(
            header("Content-Type", "application/json"),
            "application/json"
        );
        assert_eq!(header("X-Request-Id", "abc-123-def"), "abc-123-def");
        // ...but a token embedded in an ordinary header is still caught.
        assert_eq!(
            header("X-Debug", "retry with Bearer sk_live_9f8a7"),
            format!("retry with Bearer {MASK}")
        );
    }

    #[test]
    fn urls_lose_their_credential_parameters_only() {
        assert_eq!(
            url("https://api.example.com/v1/me?access_token=sk_live_abc&page=2"),
            format!("https://api.example.com/v1/me?access_token={MASK}&page=2")
        );
        assert_eq!(
            url("https://x.test/search?q=hello&lang=en"),
            "https://x.test/search?q=hello&lang=en",
            "a URL with no secrets must come through byte-identical"
        );
    }

    #[test]
    fn oauth_fragments_are_covered() {
        assert_eq!(
            url("https://app.test/callback#access_token=abc123&state=xyz"),
            format!("https://app.test/callback#access_token={MASK}&state=xyz")
        );
        // A plain anchor is not a credential.
        assert_eq!(
            url("https://docs.test/page#installation"),
            "https://docs.test/page#installation"
        );
    }

    #[test]
    fn url_userinfo_is_redacted_before_storage() {
        assert_eq!(
            url("https://alice:swordfish@app.test/private?view=full"),
            format!("https://{MASK}@app.test/private?view=full")
        );
        assert_eq!(
            url("https://alice@app.test/private"),
            format!("https://{MASK}@app.test/private")
        );
    }

    #[test]
    fn malformed_urls_still_get_redacted() {
        assert_eq!(
            url("not a url?token=secret"),
            format!("not a url?token={MASK}")
        );
        assert_eq!(url(""), "");
    }

    #[test]
    fn bearer_tokens_are_masked_in_free_text() {
        assert_eq!(
            text("failed: Authorization: Bearer sk_live_51H8xk2 rejected"),
            format!("failed: Authorization: Bearer {MASK} rejected")
        );
        assert_eq!(
            text("bearer sk_live_51H8xk2"),
            format!("bearer {MASK}"),
            "the match must be case-insensitive"
        );
    }

    #[test]
    fn english_after_the_word_bearer_is_not_a_token() {
        for phrase in [
            "the bearer of bad news",
            "Bearer ",
            "bearer is a word",
            "Bearer short",
        ] {
            assert_eq!(text(phrase), phrase, "false positive on {phrase:?}");
        }
    }

    #[test]
    fn jwts_are_masked_but_lookalikes_are_not() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVP";
        assert_eq!(text(&format!("token={jwt} ok")), format!("token={MASK} ok"));

        // Three dots but far too short to be a token.
        assert_eq!(text("eyJa.b.c"), "eyJa.b.c");
        // A word that merely starts with the same letters.
        assert_eq!(text("eyJust kidding"), "eyJust kidding");
    }

    #[test]
    fn multiple_secrets_in_one_string_are_all_masked() {
        let out = text("first Bearer sk_live_aaaaaa then Bearer sk_live_bbbbbb end");
        assert_eq!(out, format!("first Bearer {MASK} then Bearer {MASK} end"));
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = text("Bearer sk_live_abc");
        assert_eq!(text(&once), once, "re-redacting must not corrupt the mask");
        let once = url("https://x/?token=abc");
        assert_eq!(url(&once), once);
    }

    #[test]
    fn non_ascii_text_is_not_corrupted() {
        // A real token is ASCII (RFC 6750), but the text around it need not be —
        // and scanning must never split a multi-byte character.
        let out = text("ошибка авторизации: Bearer sk_live_9f8a7b6c готово ✅");
        assert_eq!(out, format!("ошибка авторизации: Bearer {MASK} готово ✅"));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());

        // A non-ASCII word after "Bearer" is not a bearer token.
        let s = "ошибка: Bearer секрет готово";
        assert_eq!(text(s), s);
    }

    #[test]
    fn masking_a_secret_never_reveals_its_length() {
        let short = text("Bearer abcdefgh");
        let long = text(&format!("Bearer {}", "a".repeat(400)));
        assert_eq!(short, long, "the mask must be fixed width");
    }
}
