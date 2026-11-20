//! `cargo run -p luaurs --example regex`
//!
//! The `Regex` library is available both from Rust and from Luau scripts.

use luaurs::luau_rt::lregex::Regex;
use luaurs::{Function, Luau, Value};

fn main() -> Result<(), luaurs::Error> {
    // 1. From Rust: compile the pattern once and reuse it.
    let regex = Regex::new(r"(?<word>[A-Za-z]+):\s*(\d+)").expect("valid pattern");
    let input = b"My Cool Id: 31235335";
    let m = regex.find(input).expect("match");
    let word = std::str::from_utf8(m.group_text(input, 1).unwrap()).unwrap();
    let id = std::str::from_utf8(m.group_text(input, 2).unwrap()).unwrap();
    println!("Rust engine: word={word} id={id}");
    println!("Rust replace: {}", String::from_utf8_lossy(&regex.replace_all(input, b"${word}#$2")));

    // 2. From Luau: the library is installed by default.
    let lua = Luau::new();
    lua.load(
        r#"
        local re = Regex.new("(?<word>[A-Za-z]+):\\s*(\\d+)")
        local m = re:Match("My Cool Id: 31235335")
        print("Luau:", m.Captures[1], m.Captures[2], re:Replace(m, "$2"))
        "#,
    )
    .exec()?;

    // 3. Call a Luau helper from Rust and read its result.
    let split: Function = lua
        .load(
            r#"
            local re = Regex.new("(\\d+)-(\\d+)")
            return function(input)
                local m = re:Match(input)
                return m.Text, m.Captures[1], m.Captures[2]
            end
            "#,
        )
        .eval()?;
    let parts: Vec<Value> = split.call(("2024-05",))?;
    println!(
        "Rust -> Luau: {}, {}, {}",
        parts[0].as_str().unwrap_or(""),
        parts[1].as_str().unwrap_or(""),
        parts[2].as_str().unwrap_or("")
    );
    Ok(())
}
