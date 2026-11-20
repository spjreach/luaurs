//! `cargo run -p luaurs --example compiled`
//!
//! Compile once, run many times, and handle errors without losing context.

use luaurs::{Luau, Value};

fn main() -> Result<(), luaurs::Error> {
    let lua = Luau::new();

    // Compile to bytecode once, then run it with different arguments.
    let bytecode = lua.compile("local a, b = ... return a * b + 1")?;
    for (a, b) in [(2.0, 3.0), (5.0, 7.0)] {
        let results = lua.load(&bytecode).bytecode().call((a, b))?;
        let answer = results.first().and_then(Value::as_number).unwrap_or(0.0);
        println!("{a} * {b} + 1 = {answer}");
    }

    // Runtime errors include the chunk name and line.
    match lua.load("local t = nil; return t.value").name("demo").exec() {
        Ok(()) => {}
        Err(error) => println!("runtime error: {error}"),
    }

    // Compile errors are reported before anything runs.
    match lua.compile("local = 1") {
        Ok(_) => {}
        Err(error) => println!("compile error: {error}"),
    }

    // Values can be passed both ways without touching the C API.
    let sum = lua.create_function(|_, args| {
        let total: f64 = args.iter().filter_map(Value::as_number).sum();
        Ok(vec![Value::from(total)])
    });
    lua.globals().set("sum", sum)?;
    let total: f64 = lua.load("return sum(1, 2, 3, 4)").eval()?;
    println!("sum = {total}");

    Ok(())
}
