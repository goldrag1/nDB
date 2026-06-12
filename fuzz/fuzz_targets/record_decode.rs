#![no_main]
//! Fuzz `Record::decode` on arbitrary bytes. The contract: any input must
//! produce a clean `Ok`/`Err` and never panic, hang, OOM, or read out of
//! bounds. libFuzzer drives coverage into every decoder branch (envelope
//! size/CRC, per-kind bodies, length/arity prefixes).

use libfuzzer_sys::fuzz_target;
use ndb_engine::record::{peek_record_kind, peek_record_size};
use ndb_engine::Record;

fuzz_target!(|data: &[u8]| {
    let _ = Record::decode(data);
    let _ = peek_record_size(data);
    let _ = peek_record_kind(data);
});
