use aionui_common::constants::{
    COOKIE_MAX_AGE_DAYS, COOKIE_NAME, CSRF_COOKIE_NAME, LEGACY_COOKIE_NAME, LEGACY_CSRF_COOKIE_NAME,
};

/// Cookie security configuration derived from the deployment environment.
#[derive(Debug, Clone)]
pub struct CookieConfig {
    /// Whether to set the `Secure` flag on cookies (HTTPS only).
    pub secure: bool,
    /// `SameSite` policy: `"Strict"` for HTTPS, `"Lax"` for HTTP.
    pub same_site: &'static str,
}

impl CookieConfig {
    /// Create cookie config from environment variables.
    ///
    /// - `AIONUI_HTTPS=true` → Secure flag, SameSite=Strict
    /// - Otherwise → no Secure flag, SameSite=Lax (for remote HTTP access)
    pub fn from_env() -> Self {
        let https = std::env::var("AIONUI_HTTPS")
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Self {
            secure: https,
            same_site: if https { "Strict" } else { "Lax" },
        }
    }

    /// Build `Set-Cookie` header value for the session token.
    ///
    /// Attributes: HttpOnly, SameSite, Secure (if HTTPS), Max-Age=30d.
    pub fn build_session_cookie(&self, token: &str) -> String {
        self.build_session_cookie_named(COOKIE_NAME, token)
    }

    /// Build primary and transition-only legacy session cookies.
    pub fn build_session_cookies(&self, token: &str) -> [String; 2] {
        [
            self.build_session_cookie_named(COOKIE_NAME, token),
            self.build_session_cookie_named(LEGACY_COOKIE_NAME, token),
        ]
    }

    fn build_session_cookie_named(&self, name: &str, token: &str) -> String {
        let max_age = u64::from(COOKIE_MAX_AGE_DAYS) * 24 * 60 * 60;
        format!(
            "{name}={token}; Path=/; HttpOnly; SameSite={}{}; Max-Age={max_age}",
            self.same_site,
            if self.secure { "; Secure" } else { "" },
        )
    }

    /// Build `Set-Cookie` header value that clears the session cookie.
    pub fn clear_session_cookie(&self) -> String {
        self.clear_session_cookie_named(COOKIE_NAME)
    }

    /// Clear primary and legacy session cookies.
    pub fn clear_session_cookies(&self) -> [String; 2] {
        [
            self.clear_session_cookie_named(COOKIE_NAME),
            self.clear_session_cookie_named(LEGACY_COOKIE_NAME),
        ]
    }

    fn clear_session_cookie_named(&self, name: &str) -> String {
        format!(
            "{name}=; Path=/; HttpOnly; SameSite={}{}; Max-Age=0",
            self.same_site,
            if self.secure { "; Secure" } else { "" },
        )
    }

    /// Build `Set-Cookie` header value for the CSRF token.
    ///
    /// NOT HttpOnly — JavaScript must read this value to include it
    /// in the `x-csrf-token` request header (Double Submit Cookie pattern).
    pub fn build_csrf_cookie(&self, token: &str) -> String {
        self.build_csrf_cookie_named(CSRF_COOKIE_NAME, token)
    }

    /// Build primary and transition-only legacy CSRF cookies.
    pub fn build_csrf_cookies(&self, token: &str) -> [String; 2] {
        [
            self.build_csrf_cookie_named(CSRF_COOKIE_NAME, token),
            self.build_csrf_cookie_named(LEGACY_CSRF_COOKIE_NAME, token),
        ]
    }

    fn build_csrf_cookie_named(&self, name: &str, token: &str) -> String {
        let max_age = u64::from(COOKIE_MAX_AGE_DAYS) * 24 * 60 * 60;
        format!(
            "{name}={token}; Path=/; SameSite={}{}; Max-Age={max_age}",
            self.same_site,
            if self.secure { "; Secure" } else { "" },
        )
    }

    /// Clear primary and legacy CSRF cookies.
    pub fn clear_csrf_cookies(&self) -> [String; 2] {
        [
            self.clear_csrf_cookie_named(CSRF_COOKIE_NAME),
            self.clear_csrf_cookie_named(LEGACY_CSRF_COOKIE_NAME),
        ]
    }

    fn clear_csrf_cookie_named(&self, name: &str) -> String {
        format!(
            "{name}=; Path=/; SameSite={}{}; Max-Age=0",
            self.same_site,
            if self.secure { "; Secure" } else { "" },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_config() -> CookieConfig {
        CookieConfig {
            secure: false,
            same_site: "Lax",
        }
    }

    fn https_config() -> CookieConfig {
        CookieConfig {
            secure: true,
            same_site: "Strict",
        }
    }

    #[test]
    fn session_cookie_http() {
        let cookie = http_config().build_session_cookie("my_token");
        assert!(cookie.contains("centaurai-session=my_token"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("Max-Age="));
        assert!(!cookie.contains("Secure"));
    }

    #[test]
    fn session_cookie_https() {
        let cookie = https_config().build_session_cookie("my_token");
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("; Secure"));
    }

    #[test]
    fn clear_session_cookie_sets_max_age_zero() {
        let cookie = http_config().clear_session_cookie();
        assert!(cookie.contains("centaurai-session="));
        assert!(cookie.contains("Max-Age=0"));
        assert!(cookie.contains("HttpOnly"));
    }

    #[test]
    fn csrf_cookie_not_http_only() {
        let cookie = http_config().build_csrf_cookie("csrf_abc");
        assert!(cookie.contains("centaurai-csrf-token=csrf_abc"));
        assert!(!cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age="));
    }

    #[test]
    fn csrf_cookie_https_has_secure() {
        let cookie = https_config().build_csrf_cookie("csrf_abc");
        assert!(cookie.contains("; Secure"));
        assert!(cookie.contains("SameSite=Strict"));
    }

    #[test]
    fn session_cookie_max_age_30_days() {
        let cookie = http_config().build_session_cookie("t");
        let expected = 30 * 24 * 60 * 60;
        assert!(cookie.contains(&format!("Max-Age={expected}")));
    }

    #[test]
    fn transition_cookie_sets_cover_primary_and_legacy_names() {
        let session = http_config().build_session_cookies("token");
        assert!(session[0].starts_with("centaurai-session=token;"));
        assert!(session[1].starts_with("aionui-session=token;"));

        let csrf = http_config().build_csrf_cookies("csrf");
        assert!(csrf[0].starts_with("centaurai-csrf-token=csrf;"));
        assert!(csrf[1].starts_with("aionui-csrf-token=csrf;"));
    }

    #[test]
    fn transition_clear_cookies_cover_both_namespaces() {
        let session = http_config().clear_session_cookies();
        let csrf = http_config().clear_csrf_cookies();
        assert!(session.iter().all(|cookie| cookie.contains("Max-Age=0")));
        assert!(csrf.iter().all(|cookie| cookie.contains("Max-Age=0")));
        assert!(session[0].starts_with("centaurai-session="));
        assert!(session[1].starts_with("aionui-session="));
        assert!(csrf[0].starts_with("centaurai-csrf-token="));
        assert!(csrf[1].starts_with("aionui-csrf-token="));
    }
}
