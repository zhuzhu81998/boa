//! Compile against a JIT-enabled engine rlib using boa_jit_linker (see the tool README).
use boa_engine::{Context, Source};

fn main() {
    let cases = [
        (
            "arithmetic",
            "let a=12; (a+3===15) && (a-3===9) && (a*3===36) && (a/3===4) && (a%5===2)",
        ),
        (
            "coercion",
            "let a={valueOf(){return 7}}; (a+5===12) && ('x'+a==='x7') && (3n+4n===7n)",
        ),
        (
            "branches",
            "let n=0; for(let i=0;i<30;i++){if(i%2===0)n+=i;else n-=i;} n===-15",
        ),
        (
            "calls",
            "function f(n){if(n===0)return 0;return n+f(n-1)} f(12)===78",
        ),
        (
            "exceptions",
            "let caught=false; try { let a={valueOf(){throw new Error('test')}}; a+1; } catch(e){caught=e.message==='test'} caught",
        ),
        (
            "drops",
            "let out=''; for(let i=0;i<30;i++){let x={a:['a','b','c']};out+=x.a[1];} out.length===30",
        ),
    ];
    for (name, script) in cases {
        let mut context = Context::default();
        let value = context
            .eval(Source::from_bytes(script))
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(value.as_boolean(), Some(true), "{name}");
        println!("{name}: passed");
    }
}
