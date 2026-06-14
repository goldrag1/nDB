//! ndb-edge — a Pingora TLS edge for nDB (sovereign, no tunnel).
//!
//! Terminates TLS on one public port and path-routes to localhost upstreams,
//! all plain HTTP (TLS stops here):
//!
//!   /mcp*                          -> MCP Streamable-HTTP server (AI agents) :9000
//!   /v1*, /arrow*, /health, /ready,
//!   /metrics, /subscribe           -> nDB data API (/v1 wire protocol)       :8742
//!   everything else                -> the data/UI upstream (e.g. static UI)  :8080
//!
//! The data API and the UI are separate upstreams so a single host can serve
//! the static explorer at `/` AND the `/v1` data API at `/v1` (the API is the
//! unified ndb-mcp-server's `--v1-bind` surface). Point `NDB_API_UPSTREAM` at
//! the data API and `NDB_DATA_UPSTREAM` at whatever serves `/` (UI or a router).
//!
//! All knobs come from the environment so one binary serves every deploy:
//!   NDB_EDGE_BIND     listen addr           (default 0.0.0.0:443)
//!   NDB_TLS_CERT      PEM fullchain path    (default /etc/ndb/fullchain.pem)
//!   NDB_TLS_KEY       PEM private key path  (default /etc/ndb/privkey.pem)
//!   NDB_API_UPSTREAM  data API host:port    (default 127.0.0.1:8742) — /v1, /arrow, /health
//!   NDB_DATA_UPSTREAM "everything else" host:port (default 127.0.0.1:8742) — UI / router
//!   NDB_MCP_UPSTREAM  MCP host:port         (default 127.0.0.1:9000)
//!   RUST_LOG          log level             (e.g. info)
//!
//! NOTE: the Pingora API (ProxyHttp, TlsSettings, add_tls_with_settings) shifts
//! between minor versions. If `cargo build` errors on a signature, check the
//! version that resolved in Cargo.lock against docs.rs/pingora and adjust.

use async_trait::async_trait;
use pingora::listeners::tls::TlsSettings;
use pingora::prelude::*;
use pingora::proxy::{ProxyHttp, Session, http_proxy_service};

/// Path-routing proxy: agents → MCP, `/v1`-family → data API, else → UI/router.
struct NdbEdge {
    api_upstream: String,
    data_upstream: String,
    mcp_upstream: String,
}

/// True for the data-API path family that must reach `NDB_API_UPSTREAM`.
fn is_api_path(path: &str) -> bool {
    path.starts_with("/v1")
        || path.starts_with("/arrow")
        || path == "/health"
        || path == "/ready"
        || path == "/metrics"
        || path.starts_with("/subscribe")
}

#[async_trait]
impl ProxyHttp for NdbEdge {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let path = session.req_header().uri.path();
        let upstream = if path.starts_with("/mcp") {
            self.mcp_upstream.as_str()
        } else if is_api_path(path) {
            self.api_upstream.as_str()
        } else {
            self.data_upstream.as_str()
        };
        // false = plain HTTP to the localhost upstream; empty SNI (no upstream TLS).
        Ok(Box::new(HttpPeer::new(upstream, false, String::new())))
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn main() {
    env_logger::init();

    let mut server = Server::new(None).expect("failed to create pingora server");
    server.bootstrap();

    let edge = NdbEdge {
        api_upstream: env_or("NDB_API_UPSTREAM", "127.0.0.1:8742"),
        data_upstream: env_or("NDB_DATA_UPSTREAM", "127.0.0.1:8742"),
        mcp_upstream: env_or("NDB_MCP_UPSTREAM", "127.0.0.1:9000"),
    };
    let bind = env_or("NDB_EDGE_BIND", "0.0.0.0:443");
    let cert = env_or("NDB_TLS_CERT", "/etc/ndb/fullchain.pem");
    let key = env_or("NDB_TLS_KEY", "/etc/ndb/privkey.pem");

    println!(
        "ndb-edge: TLS {bind}  |  /mcp* -> {}  |  /v1|/arrow|/health -> {}  |  * -> {}",
        edge.mcp_upstream, edge.api_upstream, edge.data_upstream
    );

    let mut proxy = http_proxy_service(&server.configuration, edge);
    let mut tls = TlsSettings::intermediate(&cert, &key)
        .expect("failed to load TLS cert/key — check NDB_TLS_CERT / NDB_TLS_KEY");
    tls.enable_h2();
    proxy.add_tls_with_settings(&bind, None, tls);

    server.add_service(proxy);
    server.run_forever();
}
