//! ndb-vrecall — vector-search accuracy: recall@k of nDB's `ndb.vector_search`
//! vs an exact brute-force nearest-neighbour ranking over the SAME stored
//! vectors. This measures the *index's* fidelity (does it return the true
//! nearest neighbours), independent of embedding quality.
//!
//!   ndb-vrecall 127.0.0.1:9000 <token_or_empty> [k]
//!
//! recall@k = avg over queries of |exact_topk ∩ ndb_topk| / k.
//! Each stored vector is used as a query (its own exact top-1 is itself).

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpStream;

use serde_json::{json, Value};

mod embed;
use embed::cosine;

fn post(hp: &str, token: &str, payload: &Value) -> Value {
    let body = serde_json::to_vec(payload).unwrap();
    let mut s = TcpStream::connect(hp).expect("connect mcp");
    let mut head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {hp}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if !token.is_empty() {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(&body).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let text = String::from_utf8_lossy(&resp);
    let start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    serde_json::from_str::<Value>(text[start..].trim()).unwrap_or(Value::Null)
}

fn call(hp: &str, token: &str, name: &str, args: Value) -> Value {
    post(hp, token, &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}))
        .get("result").cloned().unwrap_or(Value::Null)
}

fn main() {
    let mut a = std::env::args().skip(1);
    let hp = a.next().unwrap_or_else(|| "127.0.0.1:9000".into());
    let token = a.next().unwrap_or_default();
    let mut k: usize = a.next().and_then(|s| s.parse().ok()).unwrap_or(5);

    // Load every entity that carries a vector on property 20.
    let recs = call(&hp, &token, "ndb.iter", json!({"limit": 2000}));
    let mut items: Vec<(String, Vec<f32>)> = Vec::new();
    if let Some(arr) = recs.get("records").and_then(|r| r.as_array()) {
        for r in arr {
            if r.get("kind").and_then(|x| x.as_str()) != Some("entity") {
                continue;
            }
            let uuid = r.get("entity_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let vec = r.get("properties").and_then(|p| p.as_array()).and_then(|ps| {
                ps.iter()
                    .find(|p| p.get("prop_id").and_then(serde_json::Value::as_u64) == Some(20))
                    .and_then(|p| p.get("value"))
                    .and_then(|v| v.get("value"))
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect::<Vec<f32>>())
            });
            if let Some(v) = vec {
                if !uuid.is_empty() && !v.is_empty() {
                    items.push((uuid, v));
                }
            }
        }
    }
    let n = items.len();
    if n == 0 {
        eprintln!("no vectors found on property 20 — is NDB_VECTOR_PROP=20 set and data seeded with embeddings?");
        std::process::exit(1);
    }
    k = k.min(n);

    let mut total = 0.0f32;
    for (_, qv) in &items {
        // exact top-k by cosine (desc)
        let mut sims: Vec<(usize, f32)> = items.iter().enumerate().map(|(j, (_, v))| (j, cosine(qv, v))).collect();
        sims.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
        let exact: HashSet<&str> = sims.iter().take(k).map(|(j, _)| items[*j].0.as_str()).collect();
        // nDB index top-k
        let res = call(&hp, &token, "ndb.vector_search", json!({"property_id":20,"query":qv,"k":k,"metric":"cosine"}));
        let ndb: HashSet<String> = res.get("hits").and_then(|h| h.as_array()).map(|hits| {
            hits.iter().take(k).filter_map(|h| h.get("entity_id").and_then(|x| x.as_str()).map(String::from)).collect()
        }).unwrap_or_default();
        let inter = exact.iter().filter(|u| ndb.contains(**u)).count();
        total += inter as f32 / k as f32;
    }
    let recall = total / n as f32;
    println!("vectors={n}  queries={n}  k={k}  recall@{k}={recall:.4}");
    if (recall - 1.0).abs() < 1e-6 {
        println!("perfect recall — nDB's vector_search returns the exact nearest neighbours at this scale.");
    }
}
