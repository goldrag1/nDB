#![no_main]
//! Fuzz `Value::decode` on arbitrary bytes. Targets the per-tag length
//! prefixes (String/Bytes/Vector/Extension) where an unchecked count once
//! drove a multi-gigabyte speculative allocation — the decoder must reject
//! such inputs cleanly without ever panicking or OOM-ing.

use libfuzzer_sys::fuzz_target;
use ndb_engine::Value;

fuzz_target!(|data: &[u8]| {
    let _ = Value::decode(data);
});
