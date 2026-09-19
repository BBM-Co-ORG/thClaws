//! OIDC discovery — fetch `.well-known/openid-configuration` from the
//! issuer URL and decode the endpoints we need.
//!
//! The OIDC discovery doc tells us where to send users for login,
//! where to exchange auth codes for tokens, where to fetch the JWKS
//! for signature verification, and (sometimes) where to log out. Same
//! URL contract is honored by every major IdP — Okta, Azure AD /
//! Entra ID, Auth0, Keycloak, Google Workspace, AWS Cognito, etc.
//!
//! We fetch it lazily (first time we need it per process) and cache
//! the result for the process lifetime — discovery docs change rarely
//! enough that re-fetching every login is wasted overhead.

use serde::Deserialize;

use crate::error::{Error, Result};

/// Subset of the OIDC discovery doc we actually consume. The IdP's
/// real document has more fields; we ignore them with serde defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct DiscoveryDoc {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub userinfo_endpoint: Option<String>,
    pub jwks_uri: String,
    pub end_session_endpoint: Option<String>,
    /// Listed PKCE-supported methods (we require S256).
    pub code_challenge_methods_supported: Vec<String>,
    /// Scopes the IdP claims to support. We care about `openid` (mandatory)
    /// + `email` + `profile` for displayable identity.
    pub scopes_supported: Vec<String>,
}

/// Fetch the discovery doc for `issuer_url`. The well-known path is
/// always `<issuer>/.well-known/openid-configuration` per OIDC Core 1.0
/// §4. We strip any trailing slash on the issuer so both forms work.
pub async fn fetch(issuer_url: &str) -> Result<DiscoveryDoc> {
    let base = issuer_url.trim_end_matches('/');
    let url = format!("{base}/.well-known/openid-configuration");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Tool(format!("build http client: {e}")))?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("OIDC discovery {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Tool(format!(
            "OIDC discovery {url}: HTTP {}",
            resp.status()
        )));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Tool(format!("read discovery body: {e}")))?;
    let doc: DiscoveryDoc = serde_json::from_str(&body)
        .map_err(|e| Error::Tool(format!("parse discovery doc: {e}")))?;
    if doc.authorization_endpoint.is_empty() || doc.token_endpoint.is_empty() {
        return Err(Error::Tool(format!(
            "discovery doc at {url} is missing authorization_endpoint or token_endpoint"
        )));
    }
    if !doc.code_challenge_methods_supported.is_empty()
        && !doc
            .code_challenge_methods_supported
            .iter()
            .any(|m| m.eq_ignore_ascii_case("S256"))
    {
        return Err(Error::Tool(format!(
            "issuer {} does not advertise S256 PKCE support; this thClaws build only supports S256",
            doc.issuer
        )));
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_okta_doc() {
        let json = r#"{
            "issuer": "https://acme.okta.com",
            "authorization_endpoint": "https://acme.okta.com/oauth2/v1/authorize",
            "token_endpoint": "https://acme.okta.com/oauth2/v1/token",
            "userinfo_endpoint": "https://acme.okta.com/oauth2/v1/userinfo",
            "jwks_uri": "https://acme.okta.com/oauth2/v1/keys",
            "end_session_endpoint": "https://acme.okta.com/oauth2/v1/logout",
            "code_challenge_methods_supported": ["S256"],
            "scopes_supported": ["openid", "email", "profile", "groups"]
        }"#;
        let doc: DiscoveryDoc = serde_json::from_str(json).unwrap();
        assert_eq!(doc.issuer, "https://acme.okta.com");
        assert_eq!(doc.token_endpoint, "https://acme.okta.com/oauth2/v1/token");
        assert!(doc
            .code_challenge_methods_supported
            .contains(&"S256".to_string()));
    }

    #[test]
    fn parses_doc_without_optional_fields() {
        let json = r#"{
            "issuer": "https://example.com",
            "authorization_endpoint": "https://example.com/auth",
            "token_endpoint": "https://example.com/token",
            "jwks_uri": "https://example.com/jwks"
        }"#;
        let doc: DiscoveryDoc = serde_json::from_str(json).unwrap();
        assert!(doc.userinfo_endpoint.is_none());
        assert!(doc.end_session_endpoint.is_none());
        assert!(doc.code_challenge_methods_supported.is_empty());
    }
}
