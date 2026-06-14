//! ndb-edge — a Pingora TLS edge for nDB (sovereign, no tunnel).
//!
//! Terminates TLS on one public port and path-routes to two localhost
//! upstreams, both plain HTTP (TLS stops here):
//!
//!   /mcp*            -> the MCP Streamable-HTTP server  (AI agents)   :9000
//!   everything else  -> the nDB data API (/v1, /health) :8742  (or :8740 router)
//!
//! All knobs come from the environment so one binary serves every deploy:
//!   NDB_EDGE_BIND     listen addr           (default 0.0.0.0:443)
//!   NDB_TLS_CERT      PEM fullchain path    (default /etc/ndb/fullchain.pem)
//!   NDB_TLS_KEY       PEM private key path  (default /etc/ndb/privkey.pem)
//!   NDB_DATA_UPSTREAM data API host:port    (default 127.0.0.1:8742)
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

/// Path-routing proxy: agents to the MCP server, everything else to the data API.
struct NdbEdge {
    data_upstream: String,
    mcp_upstream: String,
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
        data_upstream: env_or("NDB_DATA_UPSTREAM", "127.0.0.1:8742"),
        mcp_upstream: env_or("NDB_MCP_UPSTREAM", "127.0.0.1:9000"),
    };
    let bind = env_or("NDB_EDGE_BIND", "0.0.0.0:443");
    let cert = env_or("NDB_TLS_CERT", "/etc/ndb/fullchain.pem");
    let key = env_or("NDB_TLS_KEY", "/etc/ndb/privkey.pem");

    println!(
        "ndb-edge: TLS {bind}  |  /mcp* -> {}  |  * -> {}",
        edge.mcp_upstream, edge.data_upstream
    );

    let mut proxy = http_proxy_service(&server.configuration, edge);
    let mut tls = TlsSettings::intermediate(&cert, &key)
        .expect("failed to load TLS cert/key — check NDB_TLS_CERT / NDB_TLS_KEY");
    tls.enable_h2();
    proxy.add_tls_with_settings(&bind, None, tls);

    server.add_service(proxy);
    server.run_forever();
}
