#![allow(missing_docs, unused_crate_dependencies)]
use boa_engine::{Context, Source};

fn main() {
    let mut context = Context::default();
    let value = context
        .eval(Source::from_bytes(
            "function add(a, b) { if (a === 0) { return 0; } return a + b; } add(20, 22);",
        ))
        .expect("JIT smoke script must execute");
    let result = value
        .to_number(&mut context)
        .expect("JIT smoke result must be numeric");
    assert_eq!(result, 42.0);
    println!("jit smoke result: {result}");
}
