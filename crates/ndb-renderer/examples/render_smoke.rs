//! Visual smoke test for the renderer.
#![allow(missing_docs)]

use ndb_engine::value::Value;
use ndb_renderer::render_text;
use ndb_slicer::Table;

fn main() {
    let t = Table {
        headers: vec!["color".into(), "count".into(), "avg".into()],
        rows: vec![
            vec![Value::String("red".into()), Value::I64(42), Value::F64(3.5)],
            vec![Value::String("blue".into()), Value::I64(7), Value::F64(1.0)],
            vec![
                Value::String("green".into()),
                Value::I64(100),
                Value::F64(2.71),
            ],
        ],
    };
    print!("{}", render_text(&t));
}
