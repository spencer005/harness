//! OAuth authorization for the MCP endpoint.
//!
//! The server registers public clients, requires PKCE, and issues short-lived,
//! single-use authorization codes after the user enters the connector password.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex},
};

use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use sonic_rs::{JsonContainerTrait, JsonValueTrait, json};
use tracing::warn;
use url::Url;

type HmacSha256 = Hmac<Sha256>;

const AUTHORIZATION_CODE_TTL_SECS: u64 = 5 * 60;
const TOKEN_TTL_SECS: u64 = 30 * 24 * 60 * 60;
const MAX_REGISTERED_CLIENTS: usize = 64;
const MAX_PENDING_CODES: usize = 128;

#[derive(Clone)]
pub struct Store {
    signing_key: Arc<[u8; 32]>,
    password: Option<Arc<String>>,
    clients: Arc<Mutex<HashMap<String, ClientRegistration>>>,
    authorization_codes: Arc<Mutex<HashMap<String, AuthorizationCode>>>,
}

#[derive(Clone)]
struct ClientRegistration {
    redirect_uris: Vec<String>,
}

struct AuthorizationCode {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    expires_at: u64,
}

struct AuthorizationRequest {
    client_id: String,
    redirect_uri: String,
    state: String,
    code_challenge: String,
}

pub enum AuthorizationOutcome {
    Page(String),
    Redirect(String),
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("auth_enabled", &self.password.is_some())
            .finish()
    }
}

impl Store {
    pub fn open(password: Option<&str>, key_file: Option<&Path>) -> Result<Self, StoreError> {
        let signing_key = if password.is_some() {
            match key_file {
                Some(path) => load_or_create_key(path)?,
                None => random_bytes(),
            }
        } else {
            random_bytes()
        };
        Ok(Self {
            signing_key: Arc::new(signing_key),
            password: password.map(|value| Arc::new(value.to_owned())),
            clients: Arc::new(Mutex::new(HashMap::new())),
            authorization_codes: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn enabled(&self) -> bool {
        self.password.is_some()
    }

    pub fn authorize(&self, header: Option<&str>) -> AuthResult {
        let Some(_) = &self.password else {
            return AuthResult::Disabled;
        };
        let Some(token) = header.and_then(bearer_token) else {
            return AuthResult::Unauthorized;
        };
        match validate_jwt(token, &self.signing_key[..]) {
            Ok(()) => AuthResult::Authorized,
            Err(error) => {
                warn!(%error, "rejected bearer token");
                AuthResult::Unauthorized
            }
        }
    }

    pub fn register_client(
        &self,
        body: &sonic_rs::Value,
    ) -> Result<sonic_rs::Value, String> {
        if !self.enabled() {
            return Err("OAuth registration is unavailable when authentication is disabled".into());
        }
        let redirect_uris = body
            .get("redirect_uris")
            .and_then(|value| value.as_array())
            .ok_or_else(|| "`redirect_uris` must be a non-empty array".to_owned())?
            .iter()
            .map(|value| {
                let uri = value
                    .as_str()
                    .ok_or_else(|| "every redirect URI must be a string".to_owned())?;
                validate_redirect_uri(uri)?;
                Ok(uri.to_owned())
            })
            .collect::<Result<Vec<_>, String>>()?;
        if redirect_uris.is_empty() {
            return Err("`redirect_uris` must contain at least one URI".into());
        }

        let client_id = random_token();
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| "OAuth client registry lock is poisoned".to_owned())?;
        if clients.len() >= MAX_REGISTERED_CLIENTS {
            return Err("OAuth client registry is full; restart the server to clear it".into());
        }
        clients.insert(
            client_id.clone(),
            ClientRegistration {
                redirect_uris: redirect_uris.clone(),
            },
        );

        Ok(json!({
            "client_id": client_id,
            "client_id_issued_at": current_timestamp(),
            "redirect_uris": redirect_uris,
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }))
    }

    fn authorization_request(
        &self,
        params: &sonic_rs::Value,
    ) -> Result<AuthorizationRequest, String> {
        let value = |name: &str| {
            params
                .get(name)
                .and_then(|value| value.as_str())
                .ok_or_else(|| format!("missing `{name}` parameter"))
        };
        if value("response_type")? != "code" {
            return Err("`response_type` must be `code`".into());
        }
        if value("code_challenge_method")? != "S256" {
            return Err("`code_challenge_method` must be `S256`".into());
        }

        let client_id = value("client_id")?.to_owned();
        let redirect_uri = value("redirect_uri")?.to_owned();
        let clients = self
            .clients
            .lock()
            .map_err(|_| "OAuth client registry lock is poisoned".to_owned())?;
        let client = clients
            .get(&client_id)
            .ok_or_else(|| "unknown OAuth client".to_owned())?;
        if !client.redirect_uris.iter().any(|uri| uri == &redirect_uri) {
            return Err("redirect URI was not registered for this client".into());
        }

        Ok(AuthorizationRequest {
            client_id,
            redirect_uri,
            state: params
                .get("state")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_owned(),
            code_challenge: value("code_challenge")?.to_owned(),
        })
    }

    pub fn begin_authorization(&self, params: &sonic_rs::Value) -> Result<String, String> {
        let request = self.authorization_request(params)?;
        Ok(consent_page(&request, None))
    }

    pub fn complete_authorization(
        &self,
        params: &sonic_rs::Value,
    ) -> Result<AuthorizationOutcome, String> {
        let request = self.authorization_request(params)?;
        let submitted = params
            .get("password")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let expected = self.password.as_deref().map(String::as_str).unwrap_or("");
        if !constant_time_eq(submitted.as_bytes(), expected.as_bytes()) {
            return Ok(AuthorizationOutcome::Page(consent_page(
                &request,
                Some("Wrong password. Try again."),
            )));
        }

        let code = random_token();
        let now = current_timestamp();
        let mut codes = self
            .authorization_codes
            .lock()
            .map_err(|_| "OAuth authorization-code lock is poisoned".to_owned())?;
        codes.retain(|_, code| code.expires_at > now);
        if codes.len() >= MAX_PENDING_CODES {
            return Err("too many pending OAuth authorizations; try again later".into());
        }
        codes.insert(
            code.clone(),
            AuthorizationCode {
                client_id: request.client_id,
                redirect_uri: request.redirect_uri.clone(),
                code_challenge: request.code_challenge,
                expires_at: now + AUTHORIZATION_CODE_TTL_SECS,
            },
        );

        let mut redirect = Url::parse(&request.redirect_uri)
            .map_err(|error| format!("invalid registered redirect URI: {error}"))?;
        redirect.query_pairs_mut().append_pair("code", &code);
        if !request.state.is_empty() {
            redirect.query_pairs_mut().append_pair("state", &request.state);
        }
        Ok(AuthorizationOutcome::Redirect(redirect.into()))
    }

    pub fn exchange_code(
        &self,
        body: &sonic_rs::Value,
        issuer: &str,
    ) -> Result<sonic_rs::Value, String> {
        if !self.enabled() {
            return Err("authentication is disabled".into());
        }
        let value = |name: &str| {
            body.get(name)
                .and_then(|value| value.as_str())
                .ok_or_else(|| format!("missing `{name}` parameter"))
        };
        if value("grant_type")? != "authorization_code" {
            return Err("only the `authorization_code` grant is supported".into());
        }

        let client_id = value("client_id")?;
        let redirect_uri = value("redirect_uri")?;
        let verifier = value("code_verifier")?;
        let code_value = value("code")?;
        let code = self
            .authorization_codes
            .lock()
            .map_err(|_| "OAuth authorization-code lock is poisoned".to_owned())?
            .remove(code_value)
            .ok_or_else(|| "invalid or already-used authorization code".to_owned())?;

        if code.expires_at <= current_timestamp() {
            return Err("authorization code expired".into());
        }
        if code.client_id != client_id || code.redirect_uri != redirect_uri {
            return Err("authorization code is not valid for this client and redirect URI".into());
        }
        let actual_challenge = base64_url_encode(&Sha256::digest(verifier.as_bytes()));
        if !constant_time_eq(actual_challenge.as_bytes(), code.code_challenge.as_bytes()) {
            return Err("PKCE verification failed".into());
        }

        let token = mint_jwt(&self.signing_key[..], issuer, TOKEN_TTL_SECS);
        Ok(json!({
            "access_token": token,
            "token_type": "Bearer",
            "expires_in": TOKEN_TTL_SECS,
            "scope": "",
        }))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("failed to read signing key file: {0}")]
    Read(#[source] std::io::Error),
    #[error("signing key file must be exactly 32 bytes, got {0}")]
    InvalidLength(usize),
    #[error(
        "signing key file permissions are too broad ({0:o}); set owner-only permissions with `chmod 600 <key-file>`"
    )]
    InsecurePermissions(u32),
    #[error("failed to create signing key file: {0}")]
    Create(#[source] std::io::Error),
    #[error("failed to write signing key file: {0}")]
    Write(#[source] std::io::Error),
}

fn load_or_create_key(path: &Path) -> Result<[u8; 32], StoreError> {
    match File::open(path) {
        Ok(file) => read_key(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(StoreError::Create)?;
            }
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(path) {
                Ok(mut file) => {
                    let key = random_bytes();
                    file.write_all(&key).map_err(StoreError::Write)?;
                    file.sync_all().map_err(StoreError::Write)?;
                    Ok(key)
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    File::open(path).map_err(StoreError::Read).and_then(read_key)
                }
                Err(error) => Err(StoreError::Create(error)),
            }
        }
        Err(error) => Err(StoreError::Read(error)),
    }
}

fn read_key(mut file: File) -> Result<[u8; 32], StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file
            .metadata()
            .map_err(StoreError::Read)?
            .permissions()
            .mode()
            & 0o777;
        if mode & 0o077 != 0 {
            return Err(StoreError::InsecurePermissions(mode));
        }
    }

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(StoreError::Read)?;
    if bytes.len() != 32 {
        return Err(StoreError::InvalidLength(bytes.len()));
    }
    let mut key = [0_u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

pub enum AuthResult {
    Authorized,
    Unauthorized,
    Disabled,
}

fn bearer_token(header: &str) -> Option<&str> {
    header.trim().strip_prefix("Bearer ").map(str::trim)
}

pub fn protected_resource_metadata(resource_url: &str) -> sonic_rs::Value {
    json!({
        "resource": resource_url,
        "authorization_servers": [strip_path(resource_url)],
        "bearer_methods_supported": ["header"],
        "resource_documentation": resource_url,
    })
}

pub fn authorization_server_metadata(base_url: &str) -> sonic_rs::Value {
    let root = format!("{base_url}/oauth");
    json!({
        "issuer": base_url,
        "authorization_endpoint": format!("{root}/authorize"),
        "token_endpoint": format!("{root}/token"),
        "registration_endpoint": format!("{root}/register"),
        "grant_types_supported": ["authorization_code"],
        "response_types_supported": ["code"],
        "token_endpoint_auth_methods_supported": ["none"],
        "code_challenge_methods_supported": ["S256"],
        "require_pushed_authorization_requests": false,
    })
}

fn consent_page(request: &AuthorizationRequest, error: Option<&str>) -> String {
    let error = error.map_or_else(String::new, |message| {
        format!(r#"<p class="error">{}</p>"#, html_escape(message))
    });
    format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>Authorize Codex Native MCP</title>
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'">
<style>
  body {{ font-family: system-ui, sans-serif; max-width: 480px; margin: 80px auto; padding: 24px; text-align: center; }}
  h1 {{ font-size: 20px; margin-bottom: 8px; }}
  p {{ color: #666; margin-bottom: 24px; }}
  .error {{ color: #d32f2f; margin-bottom: 16px; }}
  input {{ padding: 10px 14px; font-size: 16px; border: 1px solid #ccc; border-radius: 8px; width: 240px; margin-bottom: 16px; }}
  button {{ padding: 12px 32px; font-size: 16px; cursor: pointer; border: none; border-radius: 8px; background: #10a37f; color: white; }}
</style>
</head>
<body>
  <h1>Authorize "Codex Native MCP"</h1>
  <p>ChatGPT is requesting access to call tools on your local workspace. Enter the connector password to allow.</p>
  {error}
  <form method="POST" action="/oauth/authorize">
    <input type="hidden" name="response_type" value="code">
    <input type="hidden" name="redirect_uri" value="{redirect_uri}">
    <input type="hidden" name="state" value="{state}">
    <input type="hidden" name="client_id" value="{client_id}">
    <input type="hidden" name="code_challenge" value="{code_challenge}">
    <input type="hidden" name="code_challenge_method" value="S256">
    <input type="password" name="password" placeholder="Connector password" autofocus>
    <br>
    <button type="submit">Allow</button>
  </form>
</body>
</html>"#,
        redirect_uri = html_escape(&request.redirect_uri),
        state = html_escape(&request.state),
        client_id = html_escape(&request.client_id),
        code_challenge = html_escape(&request.code_challenge),
    )
}

fn validate_redirect_uri(uri: &str) -> Result<(), String> {
    let parsed = Url::parse(uri).map_err(|error| format!("invalid redirect URI: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("redirect URI must be an absolute HTTP or HTTPS URL".into());
    }
    if parsed.fragment().is_some() {
        return Err("redirect URI must not contain a fragment".into());
    }
    Ok(())
}

fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            character => escaped.push(character),
        }
    }
    escaped
}

fn mint_jwt(signing_key: &[u8], issuer: &str, ttl_secs: u64) -> String {
    let header = json!({ "alg": "HS256", "typ": "JWT" });
    let now = current_timestamp();
    let payload = json!({
        "iss": issuer,
        "iat": now,
        "exp": now + ttl_secs,
    });
    let header_b64 = base64_url_encode(&sonic_rs::to_vec(&header).expect("JWT header serializes"));
    let payload_b64 =
        base64_url_encode(&sonic_rs::to_vec(&payload).expect("JWT payload serializes"));
    let signing_input = format!("{header_b64}.{payload_b64}");

    let mut mac = HmacSha256::new_from_slice(signing_key).expect("HMAC accepts any key length");
    mac.update(signing_input.as_bytes());
    let signature = mac.finalize().into_bytes();
    format!("{signing_input}.{}", base64_url_encode(&signature))
}

fn validate_jwt(token: &str, signing_key: &[u8]) -> Result<(), String> {
    let mut parts = token.split('.');
    let header = parts.next().ok_or_else(|| "token is missing header".to_owned())?;
    let payload = parts.next().ok_or_else(|| "token is missing payload".to_owned())?;
    let signature = parts
        .next()
        .ok_or_else(|| "token is missing signature".to_owned())?;
    if parts.next().is_some() {
        return Err("token has more than three parts".into());
    }

    let header_bytes = base64_url_decode(header).map_err(|error| format!("header: {error}"))?;
    let header_value: sonic_rs::Value = sonic_rs::from_slice(&header_bytes)
        .map_err(|error| format!("header JSON: {error}"))?;
    if header_value.get("alg").and_then(|value| value.as_str()) != Some("HS256") {
        return Err("token algorithm is not HS256".into());
    }

    let signing_input = format!("{header}.{payload}");
    let mut mac = HmacSha256::new_from_slice(signing_key).map_err(|error| error.to_string())?;
    mac.update(signing_input.as_bytes());
    let expected_signature = mac.finalize().into_bytes();
    let actual_signature =
        base64_url_decode(signature).map_err(|error| format!("signature: {error}"))?;
    if !constant_time_eq(&actual_signature, &expected_signature) {
        return Err("signature mismatch".into());
    }

    let payload_bytes =
        base64_url_decode(payload).map_err(|error| format!("payload: {error}"))?;
    let payload: sonic_rs::Value = sonic_rs::from_slice(&payload_bytes)
        .map_err(|error| format!("payload JSON: {error}"))?;
    let expires_at = payload
        .get("exp")
        .and_then(|value| value.as_u64())
        .ok_or_else(|| "missing exp claim".to_owned())?;
    if current_timestamp() >= expires_at {
        return Err("token expired".into());
    }
    Ok(())
}

fn random_bytes() -> [u8; 32] {
    let mut value = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut value);
    value
}

fn random_token() -> String {
    base64_url_encode(&random_bytes())
}

fn base64_url_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn base64_url_decode(value: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|error| error.to_string())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

fn strip_path(url: &str) -> String {
    Url::parse(url)
        .ok()
        .map(|mut url| {
            url.set_path("");
            url.set_query(None);
            url.set_fragment(None);
            url.to_string().trim_end_matches('/').to_owned()
        })
        .unwrap_or_else(|| url.to_owned())
}

fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
