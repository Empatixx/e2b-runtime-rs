# envd-rs fáze 1 — implementační plán

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rust binárka `envd`, která je wire-kompatibilní s upstream E2B envd pro jádro (process, filesystem, `/files`, `/init`, auth) a projde JS SDK testy commands/files/pty.

**Architecture:** Jeden axum server na portu 49983. HTTP routy (`/init`, `/files`, …) jsou axum handlery; Connect služby `process.Process` a `filesystem.Filesystem` generuje `connectrpc-build` a připojí se jako `fallback_service`. Nad vším tower vrstva pro auth. Sdílený stav v `AppState` (`Arc`).

**Tech Stack:** Rust 1.98 (edition 2024), tokio, axum 0.8, connectrpc 0.9 + buffa 0.9, nix (users, signály), pty-process, notify, sha2/base64/subtle, dashmap, tracing.

**Spec:** `docs/superpowers/specs/2026-10-05-envd-rs-phase1-design.md`

## Global Constraints

- Port **49983**, CLI flagy jako Go (`-isnotfc -port -cgroup-root -no-cgroups -verbose -version -commit`), přijímat `-flag` i `--flag`.
- Wire formát 1:1 s `proto/` a `reference/envd.yaml` (upstream `92197909`). Názvy RPC, polí, HTTP stavových kódů a hlaviček se nemění.
- Auth hlavička `X-Access-Token`; výjimky `GET /health`, `GET /files`, `POST /files`, `POST /init`.
- Signing: `"v1_" + base64(STANDARD_NO_PAD, sha256("path:operation:username:token[:expiration]"))`.
- Keepalive: hlavička `Keepalive-Ping-Interval` (s), default **90 s**. Retence ukončených procesů **30 s**.
- Linux-only kód (PTY, inotify, setuid, `copy_file_range`, `clock_settime`) za `#[cfg(target_os = "linux")]`; na macOS se musí projekt **zkompilovat** (IDE), testy běží v Linux kontejneru (`scripts/test-linux.sh`).
- Fáze 2 routy (`/freeze`, `/unfreeze`, `/fsfreeze`, `/fsthaw`, `/collapse`, `/metrics`) vrací `501`.
- Žádné `unwrap()` v produkčním kódu mimo `main` a testy.

## Review Focus

1. **Relativní / `~` cesty a prázdná cesta** — `ListDir("")`, `Stat("~/x")`, `cwd: "rel/dir"` se musí resolvovat vůči home uživatele / default workdir přesně jako Go (`expand_and_resolve`). Test v Task 4.
2. **Pomalý / odpojený odběratel výstupu procesu** — klient se odpojí uprostřed streamu; proces musí běžet dál a ostatní odběratelé dostávat data, envd nesmí viset. Test v Task 9.
3. **Neexistující uživatel v Basic auth** — `Authorization: Basic bm9ib2R5Og==` (`nobody2:`) → `Unauthenticated` s textem `invalid username: '<name>'`, ne 500. Test v Task 3.
4. **Opakované `/init`** — druhé volání se stejným tokenem projde (204), s jiným tokenem bez MMDS → 401. Test v Task 5.
5. **Upload do neexistujícího adresáře a přepis existujícího souboru** — parenty se vytvoří, soubor se přepíše, vlastník = uživatel. Test v Task 8.

---

## Mapa souborů

```
Cargo.toml, build.rs, scripts/test-linux.sh, docker/Dockerfile
src/main.rs            CLI + start serveru
src/config.rs          parsování flagů (Config)
src/proto.rs           include_generated!
src/server.rs          build_router(state) -> axum::Router
src/state.rs           AppState, Defaults
src/execcontext.rs     resolve_default_workdir / resolve_default_username
src/permissions/mod.rs, user.rs (lookup), path.rs (expand_and_resolve, ensure_dirs)
src/api/mod.rs         HTTP routy
src/api/auth.rs        AuthLayer + Basic-auth username extension
src/api/signing.rs     generate_signature / validate_signing
src/api/init.rs, envs.rs, health.rs, files_download.rs, files_upload.rs, files_compose.rs, encoding.rs
src/host/mmds.rs       trait Mmds + HTTP klient + mock
src/services/filesystem/{mod,entry,ops,watch}.rs
src/services/process/{mod,registry,keepalive,rpc}.rs
src/services/process/handler/{mod,spawn,multiplex,env}.rs
tests/http_smoke.rs, tests/init_contract.rs, tests/files_contract.rs
```

---

### Task 1: Scaffold, codegen a server skeleton

**Files:**
- Create: `Cargo.toml`, `build.rs`, `src/main.rs`, `src/config.rs`, `src/proto.rs`, `src/server.rs`, `src/state.rs`, `src/api/mod.rs`, `src/api/health.rs`, `scripts/test-linux.sh`, `tests/http_smoke.rs`

**Interfaces:**
- Produces: `config::Config { is_not_fc: bool, port: u16, cgroup_root: String, no_cgroups: bool, verbose: bool, version: bool, commit: String }`, `config::parse(args: impl Iterator<Item=String>) -> Result<Config, String>`, `state::AppState::new(cfg: Config) -> Arc<AppState>`, `server::build_router(state: Arc<AppState>) -> axum::Router`.

- [ ] **Step 1: Toolchain**

Run: `brew install protobuf && protoc --version`
Expected: `libprotoc 3x.x`

- [ ] **Step 2: Cargo.toml + build.rs**

```toml
[package]
name = "envd"
version = "0.1.0"
edition = "2024"

[dependencies]
connectrpc = { version = "0.9", features = ["axum", "server"] }
buffa = { version = "0.9.2", features = ["json"] }
buffa-types = { version = "0.9.2", features = ["json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
axum = { version = "0.8", features = ["http1", "http2", "multipart", "tokio"] }
tower = "0.5"
tower-http = { version = "0.6", features = ["cors"] }
http = "1"
bytes = "1"
tokio = { version = "1", features = ["full"] }
tokio-util = { version = "0.7", features = ["io"] }
futures = "0.3"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
dashmap = "6"
sha2 = "0.10"
base64 = "0.22"
subtle = "2"
nix = { version = "0.29", features = ["user", "signal", "fs", "time"] }
notify = "8"
uuid = { version = "1", features = ["v4"] }
reqwest = { version = "0.12", default-features = false, features = ["json"] }
chrono = "0.4"

[target.'cfg(target_os = "linux")'.dependencies]
pty-process = { version = "0.5", features = ["async"] }

[build-dependencies]
connectrpc-build = "0.9"

[dev-dependencies]
reqwest = { version = "0.12", default-features = false, features = ["json", "multipart"] }
tempfile = "3"
```

```rust
// build.rs
fn main() {
    connectrpc_build::Config::new()
        .files(&["proto/process/process.proto", "proto/filesystem/filesystem.proto"])
        .includes(&["proto/"])
        .include_file("_connectrpc.rs")
        .compile()
        .expect("protobuf codegen failed");
}
```

```rust
// src/proto.rs
connectrpc::include_generated!();
```

- [ ] **Step 3: Prozkoumat vygenerované typy**

Run: `cargo build 2>&1 | tail -3; find target/debug/build -name '_connectrpc.rs' -path '*envd*' | head -1 | xargs dirname | xargs ls`
Zapiš si: jméno traitů (`Process`, `Filesystem`), typ owned polí pro `message` (např. `MessageField<T>`), `oneof` enumy (`process_event::Event`), `optional` → `Option<T>`. Všechny další tasky používají owned zprávy přes `request.to_owned_message()`.

- [ ] **Step 4: Failing test — /health a 501**

```rust
// tests/http_smoke.rs
use std::net::SocketAddr;

async fn spawn_server() -> SocketAddr {
    let cfg = envd::config::parse(["envd", "-isnotfc", "-port", "0"].into_iter().map(String::from)).unwrap();
    let state = envd::state::AppState::new(cfg);
    let app = envd::server::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

#[tokio::test]
async fn health_returns_204_no_store() {
    let addr = spawn_server().await;
    let res = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(res.status(), 204);
    assert_eq!(res.headers()["cache-control"], "no-store");
}

#[tokio::test]
async fn phase2_routes_return_501() {
    let addr = spawn_server().await;
    let c = reqwest::Client::new();
    for p in ["/freeze", "/unfreeze", "/fsfreeze", "/fsthaw", "/collapse"] {
        let res = c.post(format!("http://{addr}{p}")).send().await.unwrap();
        assert_eq!(res.status(), 501, "{p}");
    }
    let res = c.get(format!("http://{addr}/metrics")).send().await.unwrap();
    assert_eq!(res.status(), 501);
}

#[tokio::test]
async fn config_accepts_single_and_double_dash() {
    let c = envd::config::parse(["envd", "--port", "1234", "-isnotfc", "-no-cgroups=true"].into_iter().map(String::from)).unwrap();
    assert_eq!(c.port, 1234);
    assert!(c.is_not_fc && c.no_cgroups);
}
```

Run: `cargo test --test http_smoke` → Expected: FAIL (crate `envd` nemá lib target).

- [ ] **Step 5: Implementace**

`Cargo.toml` doplň `[lib] path = "src/lib.rs"` a `src/lib.rs` s `pub mod config; pub mod proto; pub mod server; pub mod state; pub mod api;`.

`src/config.rs` — ruční parser kompatibilní s Go `flag` (`-x`, `--x`, `-x=v`, `-x v`, bool flag bez hodnoty = true):

```rust
#[derive(Debug, Clone)]
pub struct Config {
    pub is_not_fc: bool,
    pub port: u16,
    pub cgroup_root: String,
    pub no_cgroups: bool,
    pub verbose: bool,
    pub version: bool,
    pub commit: String,
}

impl Default for Config {
    fn default() -> Self {
        Self { is_not_fc: false, port: 49983, cgroup_root: "/sys/fs/cgroup".into(), no_cgroups: false, verbose: false, version: false, commit: String::new() }
    }
}

pub fn parse(args: impl Iterator<Item = String>) -> Result<Config, String> {
    let mut cfg = Config::default();
    let mut it = args.skip(1).peekable();
    while let Some(arg) = it.next() {
        let name = arg.trim_start_matches('-');
        let (key, inline) = match name.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (name, None),
        };
        let parse_bool = |v: Option<String>| v.map_or(Ok(true), |v| v.parse::<bool>().map_err(|e| format!("-{key}: {e}")));
        match key {
            "isnotfc" => cfg.is_not_fc = parse_bool(inline)?,
            "no-cgroups" => cfg.no_cgroups = parse_bool(inline)?,
            "verbose" => cfg.verbose = parse_bool(inline)?,
            "version" => cfg.version = parse_bool(inline)?,
            "port" | "cgroup-root" | "commit" => {
                let v = inline.or_else(|| it.next()).ok_or(format!("flag needs an argument: -{key}"))?;
                match key {
                    "port" => cfg.port = v.parse().map_err(|e| format!("-port: {e}"))?,
                    "cgroup-root" => cfg.cgroup_root = v,
                    _ => cfg.commit = v,
                }
            }
            other => return Err(format!("flag provided but not defined: -{other}")),
        }
    }
    Ok(cfg)
}
```

`src/state.rs` (rozšíří Task 2):

```rust
use std::sync::Arc;
use crate::config::Config;

pub struct AppState {
    pub config: Config,
}

impl AppState {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self { config })
    }
}
```

`src/api/health.rs`:

```rust
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;

pub async fn get_health() -> impl IntoResponse {
    (StatusCode::NO_CONTENT, [(header::CACHE_CONTROL, "no-store")])
}

pub async fn not_implemented() -> StatusCode {
    StatusCode::NOT_IMPLEMENTED
}
```

`src/api/mod.rs`:

```rust
pub mod health;

use std::sync::Arc;
use axum::{routing::{get, post}, Router};
use crate::state::AppState;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/health", get(health::get_health))
        .route("/metrics", get(health::not_implemented))
        .route("/freeze", post(health::not_implemented))
        .route("/unfreeze", post(health::not_implemented))
        .route("/fsfreeze", post(health::not_implemented))
        .route("/fsthaw", post(health::not_implemented))
        .route("/collapse", post(health::not_implemented))
}
```

`src/server.rs` (Connect služby přidají Task 6 a 9):

```rust
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use crate::state::AppState;

pub fn build_router(state: Arc<AppState>) -> axum::Router {
    let connect = connectrpc::Router::new();
    crate::api::routes()
        .with_state(state)
        .fallback_service(connect.into_axum_service())
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any).expose_headers(Any))
}
```

`src/main.rs`:

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = envd::config::parse(std::env::args()).map_err(|e| { eprintln!("{e}"); e })?;
    if cfg.version { println!("{}", env!("CARGO_PKG_VERSION")); return Ok(()); }
    tracing_subscriber::fmt().with_env_filter(if cfg.verbose { "debug" } else { "info" }).init();
    let port = cfg.port;
    let app = envd::server::build_router(envd::state::AppState::new(cfg));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!(port, "envd listening");
    axum::serve(listener, app).await?;
    Ok(())
}
```

`scripts/test-linux.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
docker run --rm -v "$PWD":/src -v envd-rs-target:/src/target -v envd-rs-cargo:/usr/local/cargo/registry \
  -w /src rust:1.98 bash -c 'apt-get update -qq && apt-get install -y -qq protobuf-compiler >/dev/null && cargo test "$@"' -- "$@"
```

- [ ] **Step 6: Testy**

Run: `cargo test --test http_smoke && chmod +x scripts/test-linux.sh && scripts/test-linux.sh --test http_smoke`
Expected: 3 passed (macOS i Linux).

- [ ] **Step 7: Commit** — `git add -A && git commit -m "feat: scaffold envd server with health and phase-2 stubs"`

---

### Task 2: AppState, execcontext a signing

**Files:**
- Create: `src/execcontext.rs`, `src/api/signing.rs`
- Modify: `src/state.rs`, `src/lib.rs`, `src/api/mod.rs`

**Interfaces:**
- Produces:
  - `state::Defaults { env_vars: BTreeMap<String,String>, user: String, workdir: Option<String> }` (default user `"root"`)
  - `AppState { config, defaults: RwLock<Defaults>, access_token: RwLock<Option<String>>, initialized: AtomicBool }` (`std::sync::RwLock`)
  - `execcontext::resolve_default_workdir(workdir: &str, default: Option<&str>) -> String`
  - `execcontext::resolve_default_username(username: Option<&str>, default: &str) -> Result<String, String>`
  - `api::signing::generate_signature(path, username, operation, token, expiration: Option<i64>) -> String`
  - `api::signing::validate_signing(token_set: Option<&str>, header_token: Option<&str>, signature: Option<&str>, expiration: Option<i64>, username: Option<&str>, path: &str, operation: &str, now_unix: i64) -> Result<(), String>`

- [ ] **Step 1: Failing testy** (v `src/api/signing.rs` a `src/execcontext.rs`, `#[cfg(test)]`)

```rust
#[test]
fn signature_matches_go_vectors() {
    assert_eq!(generate_signature("/home/user/a.txt", "user", "read", "secret-token", None),
        "v1_powbkN3fI7yRlWaB5Ui2dRUwVP8UZprfSGludREq8sk");
    assert_eq!(generate_signature("/home/user/a.txt", "user", "write", "secret-token", Some(1790000000)),
        "v1_JuR4i0z5anWKCNUAkaqXV5j2MD5H1U97U5/JsV83PAs");
}

#[test]
fn validate_rules() {
    let sig = generate_signature("/p", "user", "read", "t", Some(100));
    assert!(validate_signing(None, None, None, None, None, "/p", "read", 0).is_ok(), "no token set → open");
    assert!(validate_signing(Some("t"), Some("t"), None, None, None, "/p", "read", 0).is_ok());
    assert_eq!(validate_signing(Some("t"), Some("x"), None, None, None, "/p", "read", 0).unwrap_err(), "access token present in header but does not match");
    assert_eq!(validate_signing(Some("t"), None, None, None, None, "/p", "read", 0).unwrap_err(), "missing signature query parameter");
    assert!(validate_signing(Some("t"), None, Some(&sig), Some(100), Some("user"), "/p", "read", 50).is_ok());
    assert_eq!(validate_signing(Some("t"), None, Some(&sig), Some(100), Some("user"), "/p", "read", 101).unwrap_err(), "signature is already expired");
    assert_eq!(validate_signing(Some("t"), None, Some(&sig), Some(100), Some("user"), "/p", "write", 50).unwrap_err(), "invalid signature");
}
```

```rust
#[test]
fn defaults_resolution() {
    assert_eq!(resolve_default_workdir("/a", Some("/b")), "/a");
    assert_eq!(resolve_default_workdir("", Some("/b")), "/b");
    assert_eq!(resolve_default_workdir("", None), "");
    assert_eq!(resolve_default_username(None, "root").unwrap(), "root");
    assert_eq!(resolve_default_username(Some("u"), "root").unwrap(), "u");
    assert!(resolve_default_username(None, "").is_err());
}
```

Run: `cargo test --lib` → FAIL (funkce neexistují).

- [ ] **Step 2: Implementace**

```rust
// src/api/signing.rs
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const READ: &str = "read";
pub const WRITE: &str = "write";

pub fn generate_signature(path: &str, username: &str, operation: &str, token: &str, expiration: Option<i64>) -> String {
    let raw = match expiration {
        None => format!("{path}:{operation}:{username}:{token}"),
        Some(exp) => format!("{path}:{operation}:{username}:{token}:{exp}"),
    };
    format!("v1_{}", STANDARD_NO_PAD.encode(Sha256::digest(raw.as_bytes())))
}

#[allow(clippy::too_many_arguments)]
pub fn validate_signing(token_set: Option<&str>, header_token: Option<&str>, signature: Option<&str>, expiration: Option<i64>,
                        username: Option<&str>, path: &str, operation: &str, now_unix: i64) -> Result<(), String> {
    let Some(token) = token_set else { return Ok(()) };
    if let Some(h) = header_token.filter(|h| !h.is_empty()) {
        return if bool::from(h.as_bytes().ct_eq(token.as_bytes())) { Ok(()) }
               else { Err("access token present in header but does not match".into()) };
    }
    let signature = signature.ok_or("missing signature query parameter")?;
    let expected = generate_signature(path, username.unwrap_or(""), operation, token, expiration);
    if !bool::from(expected.as_bytes().ct_eq(signature.as_bytes())) {
        return Err("invalid signature".into());
    }
    if expiration.is_some_and(|exp| exp < now_unix) {
        return Err("signature is already expired".into());
    }
    Ok(())
}
```

```rust
// src/execcontext.rs
pub const BUILTIN_DEFAULT_USER: &str = "root";

pub fn resolve_default_workdir(workdir: &str, default: Option<&str>) -> String {
    if !workdir.is_empty() { return workdir.to_string(); }
    default.unwrap_or("").to_string()
}

pub fn resolve_default_username(username: Option<&str>, default: &str) -> Result<String, String> {
    if let Some(u) = username { return Ok(u.to_string()); }
    if !default.is_empty() { return Ok(default.to_string()); }
    Err("username not provided".into())
}
```

`src/state.rs`:

```rust
use std::{collections::BTreeMap, sync::{atomic::AtomicBool, Arc, RwLock}};
use crate::{config::Config, execcontext::BUILTIN_DEFAULT_USER};

#[derive(Debug, Clone)]
pub struct Defaults {
    pub env_vars: BTreeMap<String, String>,
    pub user: String,
    pub workdir: Option<String>,
}

impl Default for Defaults {
    fn default() -> Self { Self { env_vars: BTreeMap::new(), user: BUILTIN_DEFAULT_USER.into(), workdir: None } }
}

pub struct AppState {
    pub config: Config,
    pub defaults: RwLock<Defaults>,
    pub access_token: RwLock<Option<String>>,
    pub initialized: AtomicBool,
}

impl AppState {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self { config, defaults: RwLock::default(), access_token: RwLock::new(None), initialized: AtomicBool::new(false) })
    }
    pub fn defaults(&self) -> Defaults { self.defaults.read().expect("defaults lock poisoned").clone() }
    pub fn access_token(&self) -> Option<String> { self.access_token.read().expect("token lock poisoned").clone() }
}
```

- [ ] **Step 3: Testy** — `cargo test --lib` → PASS
- [ ] **Step 4: Commit** — `git commit -am "feat: app state, execcontext defaults and file signing"`

---

### Task 3: Auth vrstva

**Files:**
- Create: `src/api/auth.rs`
- Modify: `src/server.rs`, `tests/http_smoke.rs`

**Interfaces:**
- Consumes: `AppState::access_token()`
- Produces: `api::auth::require_token(State<Arc<AppState>>, Request, Next) -> Response` (axum `from_fn_with_state`); `api::auth::BasicUser(pub Option<String>)` request extension (username z `Authorization: Basic`, bez validace — validuje Task 4 `permissions::get_auth_user`).

- [ ] **Step 1: Failing test** (do `tests/http_smoke.rs`; přidej helper `set_token(&state, "t")` přes `*state.access_token.write().unwrap() = Some("t".into())` — upravit `spawn_server` aby vracel i `Arc<AppState>`)

```rust
#[tokio::test]
async fn token_required_except_excluded_paths() {
    let (addr, state) = spawn_server_with_state().await;
    *state.access_token.write().unwrap() = Some("t".into());
    let c = reqwest::Client::new();
    assert_eq!(c.get(format!("http://{addr}/health")).send().await.unwrap().status(), 204);
    let res = c.get(format!("http://{addr}/envs")).send().await.unwrap();
    assert_eq!(res.status(), 401);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["message"], "unauthorized access, please provide a valid access token or method signing if supported");
    assert_eq!(body["code"], 401);
    assert_ne!(c.get(format!("http://{addr}/envs")).header("X-Access-Token", "t").send().await.unwrap().status(), 401);
}
```

Run → FAIL (401 nevrací).

- [ ] **Step 2: Implementace**

```rust
// src/api/auth.rs
use std::sync::Arc;
use axum::{extract::{Request, State}, http::{header, StatusCode}, middleware::Next, response::{IntoResponse, Response}, Json};
use base64::{engine::general_purpose::STANDARD, Engine};
use subtle::ConstantTimeEq;
use crate::state::AppState;

const ACCESS_TOKEN_HEADER: &str = "x-access-token";
const EXCLUDED: &[(&str, &str)] = &[("GET", "/health"), ("GET", "/files"), ("POST", "/files"), ("POST", "/init")];

#[derive(Clone, Debug)]
pub struct BasicUser(pub Option<String>);

pub fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "code": status.as_u16(), "message": message }))).into_response()
}

pub async fn require_token(State(state): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let user = basic_username(req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()));
    req.extensions_mut().insert(BasicUser(user));
    let Some(token) = state.access_token() else { return next.run(req).await };
    let excluded = EXCLUDED.iter().any(|(m, p)| req.method().as_str() == *m && req.uri().path() == *p);
    let provided = req.headers().get(ACCESS_TOKEN_HEADER).map(|v| v.as_bytes()).unwrap_or_default();
    if !excluded && !bool::from(provided.ct_eq(token.as_bytes())) {
        tracing::error!("Trying to access secured envd without correct access token");
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized access, please provide a valid access token or method signing if supported");
    }
    next.run(req).await
}

fn basic_username(value: Option<&str>) -> Option<String> {
    let encoded = value?.strip_prefix("Basic ")?;
    let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
    Some(decoded.split_once(':').map_or(decoded.clone(), |(u, _)| u.to_string()))
}
```

`src/server.rs`: obal celý router (včetně Connect fallbacku) `.layer(axum::middleware::from_fn_with_state(state.clone(), crate::api::auth::require_token))` před CORS layer (CORS musí být vnější, aby preflight `OPTIONS` prošel bez tokenu).

- [ ] **Step 3: Testy** — `cargo test` → PASS. Ověř, že error JSON tvar sedí s Go `jsonError` (`reference/envd.yaml` schema `Error`: `{message, code}`).
- [ ] **Step 4: Commit** — `git commit -am "feat: access token auth layer and basic-auth user extraction"`

---

### Task 4: permissions — uživatelé a cesty

**Files:**
- Create: `src/permissions/mod.rs`, `src/permissions/user.rs`, `src/permissions/path.rs`

**Interfaces:**
- Produces:
  - `permissions::user::UserInfo { name: String, uid: u32, gid: u32, home: PathBuf, groups: Vec<u32> }`
  - `permissions::user::lookup(name: &str) -> Result<UserInfo, String>` (nix `User::from_name` + `getgrouplist` na Linuxu)
  - `permissions::get_auth_user(basic: Option<&str>, default_user: &str) -> Result<UserInfo, connectrpc::ConnectError>` — `invalid username: '<n>'` / `no user specified` / `invalid default user: '<n>'` (vše `Unauthenticated`)
  - `permissions::path::expand_and_resolve(path: &str, user: &UserInfo, default: Option<&str>) -> Result<PathBuf, String>`
  - `permissions::path::ensure_dirs(path: &Path, uid: u32, gid: u32) -> std::io::Result<()>` (mkdir 0o755 + chown jen nově vytvořených)

- [ ] **Step 1: Failing testy** (`path.rs`, `#[cfg(test)]`)

```rust
fn u() -> UserInfo { UserInfo { name: "user".into(), uid: 1000, gid: 1000, home: "/home/user".into(), groups: vec![] } }

#[test]
fn expand_and_resolve_matches_go() {
    assert_eq!(expand_and_resolve("/abs", &u(), Some("/w")).unwrap(), PathBuf::from("/abs"));
    assert_eq!(expand_and_resolve("", &u(), Some("/w")).unwrap(), PathBuf::from("/w"));
    assert_eq!(expand_and_resolve("", &u(), None).unwrap(), PathBuf::from("/home/user"));
    assert_eq!(expand_and_resolve("~", &u(), None).unwrap(), PathBuf::from("/home/user"));
    assert_eq!(expand_and_resolve("~/x", &u(), None).unwrap(), PathBuf::from("/home/user/x"));
    assert_eq!(expand_and_resolve("rel/../dir", &u(), None).unwrap(), PathBuf::from("/home/user/dir"));
    assert_eq!(expand_and_resolve("~other/x", &u(), None).unwrap_err(),
        "failed to expand path '~other/x' for user 'user': cannot expand user-specific home dir");
}
```

Plus v `mod.rs`: `get_auth_user(Some("definitely-missing-user"), "root")` → err s message `invalid username: 'definitely-missing-user'` a kódem `Unauthenticated`; `get_auth_user(None, "root")` → `name == "root"`.

- [ ] **Step 2: Implementace** `path.rs`

```rust
use std::path::{Component, Path, PathBuf};
use super::user::UserInfo;
use crate::execcontext::resolve_default_workdir;

fn expand(path: &str, home: &Path) -> Result<PathBuf, String> {
    match path.strip_prefix('~') {
        None => Ok(PathBuf::from(path)),
        Some(rest) if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') => Ok(clean(&home.join(rest.trim_start_matches(['/', '\\'])))),
        Some(_) => Err("cannot expand user-specific home dir".into()),
    }
}

/// Lexical clean like Go's filepath.Clean (no symlink resolution).
fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => { out.pop(); }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() { PathBuf::from("/") } else { out }
}

pub fn expand_and_resolve(path: &str, user: &UserInfo, default: Option<&str>) -> Result<PathBuf, String> {
    let path = resolve_default_workdir(path, default);
    let expanded = expand(&path, &user.home)
        .map_err(|e| format!("failed to expand path '{path}' for user '{}': {e}", user.name))?;
    if expanded.is_absolute() { return Ok(clean(&expanded)); }
    Ok(clean(&user.home.join(expanded)))
}
```

`user.rs` přes `nix::unistd::{User, getgrouplist}` (getgrouplist jen `cfg(linux)`, jinde prázdné groups). `ensure_dirs` iteruje `path.ancestors()` od kořene, `mkdir` 0o755 a `nix::unistd::chown` jen na nově vytvořené.

- [ ] **Step 3: Testy** — `cargo test --lib permissions` → PASS (lokálně i `scripts/test-linux.sh --lib`)
- [ ] **Step 4: Commit** — `git commit -am "feat: user lookup and go-compatible path resolution"`

---

### Task 5: `/init`, `/envs` a MMDS

**Files:**
- Create: `src/host/mod.rs`, `src/host/mmds.rs`, `src/api/init.rs`, `src/api/envs.rs`, `tests/init_contract.rs`
- Modify: `src/state.rs` (pole `mmds: Arc<dyn Mmds>`), `src/api/mod.rs`

**Interfaces:**
- Produces:
  - `host::mmds::Mmds` trait: `async fn access_token_hash(&self) -> Result<Option<String>, String>` (`None` = MMDS nedostupné). Implementace `HttpMmds` (Firecracker `http://169.254.169.254`, token `PUT /latest/api/token` + `GET` JSON klíč `accessTokenHash`) a `NoMmds` (vždy `None`, použita s `-isnotfc`). `AppState::with_mmds(cfg, Arc<dyn Mmds>)` pro testy.
  - `host::hash_access_token(token: &str) -> String` = shodné s Go `keys.HashAccessToken` (ověřit v `packages/shared/pkg/keys` upstream: sha256 + hex/base64 — zjistit při implementaci a zapsat test vektor).
  - `POST /init` → 204 / 401 (`access token validation failed`, `access token reset not authorized`) / 400.
  - `GET /envs` → `200 {"KEY":"VAL"}`.

- [ ] **Step 1: Ověřit upstream hash** — `grep -n "func HashAccessToken" -A8 <upstream>/packages/shared/pkg/keys/*.go`, zapsat přesnou formuli do doc-commentu `hash_access_token` + test vektor (spočítat pythonem).

- [ ] **Step 2: Failing kontraktové testy** (`tests/init_contract.rs`)

```rust
#[tokio::test]
async fn init_sets_envs_user_and_token() {
    let (addr, _) = spawn_server_with_state().await;
    let c = reqwest::Client::new();
    let res = c.post(format!("http://{addr}/init")).json(&serde_json::json!({
        "envVars": {"FOO": "bar"}, "accessToken": "t1", "defaultUser": "root", "defaultWorkdir": "/tmp"
    })).send().await.unwrap();
    assert_eq!(res.status(), 204);
    let envs: serde_json::Value = c.get(format!("http://{addr}/envs")).header("X-Access-Token", "t1").send().await.unwrap().json().await.unwrap();
    assert_eq!(envs["FOO"], "bar");
}

#[tokio::test]
async fn init_token_rotation_rules() {
    let (addr, _) = spawn_server_with_state().await;
    let c = reqwest::Client::new();
    let post = |tok: &'static str| c.post(format!("http://{addr}/init")).json(&serde_json::json!({"accessToken": tok})).send();
    assert_eq!(post("t1").await.unwrap().status(), 204, "first init sets token");
    assert_eq!(post("t1").await.unwrap().status(), 204, "same token again ok");
    assert_eq!(post("t2").await.unwrap().status(), 401, "different token without MMDS rejected");
}
```

Unit test v `init.rs` s `MockMmds(Some(hash_access_token("t2")))`: rotace na `t2` projde.

- [ ] **Step 3: Implementace** — `validate_init_access_token` přesně dle upstream `init.go:48-114`:
  1. request token == stávající → OK
  2. MMDS hash == hash(request) → OK
  3. žádný stávající token a MMDS neexistuje → OK (první nastavení)
  4. stávající nastaven a MMDS neexistuje → `access token reset not authorized`
  5. jinak `access token validation failed`
  Pak `SetData`: env vars (merge), `defaultUser`, `defaultWorkdir`, token; `timestamp` → `nix::time::clock_settime(CLOCK_REALTIME)` jen `!is_not_fc` a jen když se liší o > 50 ms (`cfg(linux)`); `hyperloopIP` → přepis řádku `events.e2b.local` v `/etc/hosts`; `volumeMounts`/`caBundle` → `tracing::warn!("… not supported in phase 1")`. `initialized.store(true)`.

- [ ] **Step 4: Testy** — `cargo test --test init_contract --lib` → PASS
- [ ] **Step 5: Commit** — `git commit -am "feat: /init with token rotation rules, /envs, MMDS client"`

---

### Task 6: Filesystem — unární RPC

**Files:**
- Create: `src/services/mod.rs`, `src/services/filesystem/{mod.rs,entry.rs,ops.rs}`
- Modify: `src/server.rs` (registrace služby), `src/lib.rs`

**Interfaces:**
- Consumes: `permissions::get_auth_user`, `expand_and_resolve`, `ensure_dirs`, `BasicUser` (z `RequestContext` extensions — ověřit, že axum extension z Task 3 je dostupná přes `ctx`; pokud ne, číst `Authorization` hlavičku přímo z `ctx.header(...)` stejnou funkcí `basic_username`, kterou zveřejni jako `pub`).
- Produces: `services::filesystem::FilesystemService { state: Arc<AppState> }` implementující vygenerovaný trait `Filesystem`; `entry::entry_info(path: &Path) -> Result<EntryInfo, ConnectError>`; `entry::permissions_string(mode: u32, file_type) -> String`.

- [ ] **Step 1: Failing testy** (`entry.rs`, `ops.rs` unit testy nad `tempfile::TempDir`)

```rust
#[test]
fn permissions_string_like_go_filemode() {
    assert_eq!(permissions_string(0o755, Kind::Dir), "drwxr-xr-x");
    assert_eq!(permissions_string(0o644, Kind::File), "-rw-r--r--");
    assert_eq!(permissions_string(0o777, Kind::Symlink), "Lrwxrwxrwx");
}
```

(Go `os.FileMode.String()` používá `L` pro symlink a `d` pro adresář — ověř v upstream `filesystem/utils.go` a případně uprav.)

Další testy: `stat` neexistující → `NotFound` s textem z upstream `stat.go`; `make_dir` existující → `AlreadyExists`; `list_dir(depth=2)` vrací vnořené položky; `move` vrací `EntryInfo` cíle; `remove` adresáře rekurzivně.

- [ ] **Step 2: Implementace** — port `stat.go`, `dir.go`, `move.go`, `remove.go`, `utils.go` 1:1 (chybové texty kopírovat doslova). Owner/group přes `nix::unistd::{User,Group}::from_uid/from_gid`.
- [ ] **Step 3: Registrace** — `server.rs`: `let connect = Arc::new(FilesystemService::new(state.clone())).register(connectrpc::Router::new());`
- [ ] **Step 4: Testy** — `scripts/test-linux.sh --lib filesystem` → PASS
- [ ] **Step 5: Commit** — `git commit -am "feat: filesystem stat/makedir/move/listdir/remove"`

---

### Task 7: Filesystem — watchery

**Files:**
- Create: `src/services/filesystem/watch.rs`
- Modify: `src/services/filesystem/mod.rs`, `src/services/process/keepalive.rs` (vytvořit zde, sdílí Task 9)

**Interfaces:**
- Produces: `keepalive::interval_from(ctx: &RequestContext) -> Duration` (hlavička `Keepalive-Ping-Interval`, default 90 s); `watch::Watchers` (`DashMap<String, WatcherHandle>`), `watch::map_event(notify::Event, root: &Path) -> Vec<FilesystemEvent>`.

- [ ] **Step 1: Failing testy** — `map_event` pro create/modify(data)/remove/rename/chmod → `EventType` + relativní `name`; integrační (`cfg(linux)`): `CreateWatcher` na tempdir, vytvoř soubor, `GetWatcherEvents` vrátí `CREATE` s `name == "a.txt"`, druhé volání vrátí prázdno; `RemoveWatcher` neznámého id → `NotFound`.
- [ ] **Step 2: Implementace** — `notify::RecommendedWatcher` → `tokio::mpsc`; `WatchDir` stream: nejdřív `StartEvent`, pak `select!` mezi událostmi a keepalive tickerem; síťové FS: `nix::sys::statfs` magic NFS `0x6969`, FUSE `0x65735546`, CIFS `0xFF534D42`, SMB2 `0xFE534D42` + `allow_network_mounts=false` → chyba s textem z upstream `watch.go`.
- [ ] **Step 3: Testy** — `scripts/test-linux.sh --lib watch` → PASS
- [ ] **Step 4: Commit** — `git commit -am "feat: filesystem watchers (stream and polling)"`

---

### Task 8: `/files` download, upload, compose

**Files:**
- Create: `src/api/files_download.rs`, `src/api/files_upload.rs`, `src/api/files_compose.rs`, `src/api/encoding.rs`, `tests/files_contract.rs`
- Modify: `src/api/mod.rs`

**Interfaces:**
- Consumes: `signing::validate_signing`, `get_auth_user`, `expand_and_resolve`, `ensure_dirs`, `entry::entry_info`
- Produces: `GET /files?path&username&signature&signature_expiration`, `POST /files` (multipart pole `file`, query `path`), `POST /files/compose` (JSON dle `reference/envd.yaml`).

- [ ] **Step 1: Failing kontraktové testy** (`tests/files_contract.rs`, běží jako root v kontejneru)

```rust
#[tokio::test]
async fn upload_creates_parents_overwrites_and_downloads() {
    let (addr, _) = spawn_server_with_state().await;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("a/b/c.txt");
    let c = reqwest::Client::new();
    for content in ["one", "two"] {
        let form = reqwest::multipart::Form::new().part("file", reqwest::multipart::Part::text(content).file_name("c.txt"));
        let res = c.post(format!("http://{addr}/files?path={}", target.display())).multipart(form).send().await.unwrap();
        assert_eq!(res.status(), 200);
        let entries: serde_json::Value = res.json().await.unwrap();
        assert_eq!(entries[0]["path"], target.display().to_string());
    }
    let body = c.get(format!("http://{addr}/files?path={}", target.display())).send().await.unwrap().text().await.unwrap();
    assert_eq!(body, "two");
}

#[tokio::test]
async fn download_requires_signature_when_token_set() {
    let (addr, state) = spawn_server_with_state().await;
    *state.access_token.write().unwrap() = Some("t".into());
    let res = reqwest::get(format!("http://{addr}/files?path=/etc/hostname")).await.unwrap();
    assert_eq!(res.status(), 401);
    let sig = envd::api::signing::generate_signature("/etc/hostname", "", "read", "t", None);
    let res = reqwest::get(format!("http://{addr}/files?path=/etc/hostname&signature={sig}")).await.unwrap();
    assert_eq!(res.status(), 200);
}
```

Plus: compose dvou souborů → obsah spojený, zdroje smazané; download neexistujícího → 404 s JSON `{code,message}`.

- [ ] **Step 2: Implementace** — port `download.go`, `upload.go`, `compose.go`, `encoding.go` (stavové kódy a texty doslova). Download: `tokio::fs::File` → `ReaderStream`, `Content-Disposition: inline; filename="…"`, gzip dle `Accept-Encoding` (přidej `async-compression` jen pokud upstream gzip skutečně dělá — ověř v `encoding.go`). Upload: streamovat multipart field do souboru (žádné bufferování celého souboru), `ensure_dirs`, `chown`. Compose: `nix::fcntl::copy_file_range` (`cfg(linux)`, fallback `tokio::io::copy`).
- [ ] **Step 3: Testy** — `scripts/test-linux.sh --test files_contract` → PASS
- [ ] **Step 4: Commit** — `git commit -am "feat: /files download, upload and compose"`

---

### Task 9: Process — spawn, multiplex, Start/Connect

**Files:**
- Create: `src/services/process/{mod.rs,registry.rs,rpc.rs}`, `src/services/process/handler/{mod.rs,spawn.rs,multiplex.rs,env.rs}`
- Modify: `src/server.rs`

**Interfaces:**
- Consumes: `get_auth_user`, `expand_and_resolve`, `keepalive::interval_from`, `AppState::defaults()`
- Produces:
  - `handler::env::build_env(path: &str, user: &UserInfo, global: &BTreeMap<String,String>, req: &HashMap<String,String>) -> Vec<(String,String)>` (pořadí dle specu, duplicity ponechány)
  - `handler::multiplex::Multiplex<T: Clone + Send + 'static>`: `new(buffer: usize) -> (Self, mpsc::Sender<T>)`, `subscribe(&self) -> mpsc::Receiver<T>`; drop receiveru = odhlášení
  - `handler::ProcessHandle { pid: u32, tag: Option<String>, config: ProcessConfig, data: Multiplex<DataEvent>, end: watch::Receiver<Option<EndEvent>>, stdin: Mutex<Option<Stdin>>, pty: Option<PtyControl> }`
  - `registry::Registry { running: DashMap<u32, Arc<ProcessHandle>>, terminated: DashMap<u32, RetainedExit> }`, `find(&self, sel: &ProcessSelector) -> Lookup` (`Running(Arc<…>) | Exited(EndEvent) | NotFound`)

- [ ] **Step 1: Failing testy**

```rust
// env.rs
#[test]
fn env_layering_order() {
    let user = UserInfo { name: "user".into(), uid: 1, gid: 1, home: "/home/user".into(), groups: vec![] };
    let global = BTreeMap::from([("A".into(), "g".into())]);
    let req = HashMap::from([("A".to_string(), "r".to_string())]);
    let env = build_env("/usr/bin", &user, &global, &req);
    assert_eq!(&env[..4], &[("PATH".into(), "/usr/bin".into()), ("HOME".into(), "/home/user".into()), ("USER".into(), "user".into()), ("LOGNAME".into(), "user".into())]);
    assert_eq!(&env[4..], &[("A".into(), "g".into()), ("A".into(), "r".into())]);
}

// multiplex.rs
#[tokio::test]
async fn slow_subscriber_gets_everything_and_dropped_one_does_not_block() {
    let (mux, tx) = Multiplex::<u32>::new(1);
    let mut slow = mux.subscribe();
    let dropped = mux.subscribe();
    drop(dropped);
    let producer = tokio::spawn(async move { for i in 0..100 { tx.send(i).await.unwrap(); } });
    let mut got = vec![];
    while let Some(v) = slow.recv().await { got.push(v); tokio::task::yield_now().await; }
    producer.await.unwrap();
    assert_eq!(got, (0..100).collect::<Vec<_>>());
}
```

Integrační (`cfg(linux)`, `tests/process_contract.rs` přes Connect JSON `POST /process.Process/Start` s `Content-Type: application/connect+json`): `echo hi` → stream obsahuje `start.pid`, `data.stdout == base64("hi\n")`, `end.exitCode == 0`; `cwd: "/nope"` → error `invalid_argument` `cwd '/nope' does not exist`; klient zavře stream po `start` u `sleep 1; echo x` → druhý `Connect` podle pid dostane `x` a `end`.

- [ ] **Step 2: Multiplex — připravit kostru s `TODO(human)`** (fan-out smyčka je learning úkol pro uživatele: backpressure vs. přeskakování zavřených odběratelů)

- [ ] **Step 3: Spawn** — `/bin/sh -c '<wrapper>' -- cmd args…` (wrapper string zkopíruj z upstream `handler.go:200-202`, včetně `ioniceNicePrefix`); `Command::uid/gid/groups` (`std::os::unix::process::CommandExt`), `env_clear()` + `build_env`; PTY přes `pty_process::Command` s `Size::new(rows, cols)`; čtecí tasky stdout/stderr/pty → `Multiplex`; wait task → `EndEvent{exit_code, exited, status: ExitStatus Display ("exit status 0" formát jako Go!), error}` → `end` watch + přesun do `terminated` s TTL 30 s.
- [ ] **Step 4: Start/Connect stream** — `ServiceStream` z `async_stream`-like `futures::stream::unfold`: `StartEvent` → `select!{ data.recv(), end.changed(), keepalive.tick() }`; po každém odeslání reset tickeru.
- [ ] **Step 5: Testy** — `scripts/test-linux.sh --lib process && scripts/test-linux.sh --test process_contract` → PASS
- [ ] **Step 6: Commit** — `git commit -am "feat: process start/connect with multiplexed output and pty"`

---

### Task 10: Process — ostatní RPC

**Files:**
- Modify: `src/services/process/rpc.rs`, `registry.rs`

**Interfaces:**
- Consumes: `Registry::find`, `ProcessHandle`

- [ ] **Step 1: Failing testy** (`tests/process_contract.rs`): `List` obsahuje proces s tagem; `SendInput` do `cat` (stdin=true) → výstup echo; `StreamInput` (client stream, HTTP/2 h2c — reqwest `http2_prior_knowledge`) totéž; `CloseStdin` → `cat` skončí s 0; `SendSignal SIGTERM` → `end.exited == false`, status obsahuje `signal: terminated`; `Update` PTY resize → `stty size` v PTY vrátí nové `rows cols`; selektor na neznámý pid → `not_found` `process with pid N not found` (text z upstream `connect.go`); ukončený proces do 30 s → `Connect` vrátí rovnou `end`.
- [ ] **Step 2: Implementace** dle upstream `list.go`, `input.go`, `signal.go`, `update.go`; SIGTERM/SIGKILL nejdřív zruší čtecí tasky (`CancellationToken`), pak `nix::sys::signal::kill`.
- [ ] **Step 3: Testy** → PASS
- [ ] **Step 4: Commit** — `git commit -am "feat: process list, input, signals, pty resize, close stdin"`

---

### Task 11: Docker image a E2B JS SDK testy

**Files:**
- Create: `docker/Dockerfile`, `scripts/sdk-tests.sh`, `docs/sdk-tests.md`

- [ ] **Step 1: Ověřit debug režim SDK** — v `e2b-dev/E2B` `packages/js-sdk/src/connectionConfig.ts` najít `debug` / `E2B_DEBUG` a jak skládá envd URL (`http://localhost:49983`). Zapsat do `docs/sdk-tests.md`.
- [ ] **Step 2: Dockerfile** — multi-stage: `rust:1.98` build s `protobuf-compiler` → runtime z upstream `debug.Dockerfile` báze (uživatel `user` s home `/home/user`, `sudo`, `python3`), `ENTRYPOINT ["/usr/bin/envd", "-isnotfc", "-verbose"]`, `EXPOSE 49983`.
- [ ] **Step 3: Skript** — `scripts/sdk-tests.sh`: build + `docker run -d -p 49983:49983`, clone E2B SDK (pinned commit), `pnpm install`, `E2B_DEBUG=true pnpm vitest run tests/sandbox/commands tests/sandbox/files tests/sandbox/pty`.
- [ ] **Step 4: Spustit, opravit rozdíly** — každá chyba = nový failing unit/kontraktový test v příslušném modulu, pak fix.
- [ ] **Step 5: Commit** — `git commit -am "test: docker image and E2B JS SDK compatibility run"`
