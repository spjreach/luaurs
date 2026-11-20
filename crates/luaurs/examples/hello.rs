//! `cargo run --release -p luaurs --example hello`

use luaurs::{Error, Function, Luau, Value};

fn main() -> Result<(), Error> {
    let lua = Luau::new();

    // run code
    lua.load(r#"print("hello from " .. _VERSION)"#).exec()?;

    // get values back
    let answer: f64 = lua.load("return 6 * 7").eval()?;
    println!("6 * 7 = {answer}");

    // expose a Rust function
    let shout = lua.create_function(|_, args| {
        let s = args.first().and_then(Value::as_str).unwrap_or("").to_uppercase();
        Ok(vec![Value::from(s)])
    });
    lua.globals().set("shout", shout)?;
    lua.load(r#"print(shout("rust and luau"))"#).exec()?;

    // tables both ways
    let config = lua.create_table();
    config.set("width", 1280)?;
    config.set("height", 720)?;
    lua.globals().set("config", config)?;
    let area: f64 = lua.load("return config.width * config.height").eval()?;
    println!("area = {area}");

    // call a Luau function from Rust
    let fib: Function = lua.load("local function fib(n) return n < 2 and n or fib(n - 1) + fib(n - 2) end return fib").eval()?;
    let fib30: f64 = fib.call_get((30,))?;
    println!("fib(30) = {fib30}");

    // the task library and signals
    lua.load(
        r#"
        local ready = Signal.new()
        task.delay(0.05, function() ready:Fire("done") end)
        print("waiting...")
        print("signal said", ready:Wait())
        "#,
    )
    .exec()?;
    Ok(())
}
