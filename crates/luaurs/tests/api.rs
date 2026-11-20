use luaurs::{Error, Function, Luau, Options, Table, UserData, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[test]
fn exec_and_eval() {
    let lua = Luau::new();
    lua.load("x = 40 + 2").exec().unwrap();
    let x: f64 = lua.get_global("x").unwrap();
    assert_eq!(x, 42.0);
    let s: String = lua.load("return ('ab'):rep(3)").eval().unwrap();
    assert_eq!(s, "ababab");
    let v: Vec<Value> = lua.load("return 1, 'two', true, nil").call(()).unwrap();
    assert_eq!(v.len(), 4);
    assert_eq!(v[1].as_str(), Some("two"));
}

#[test]
fn chunk_arguments() {
    let lua = Luau::new();
    let r: f64 = lua
        .load("local a, b = ... return a * b")
        .call((6, 7))
        .unwrap()[0]
        .as_number()
        .unwrap();
    assert_eq!(r, 42.0);
}

#[test]
fn errors() {
    let lua = Luau::new();
    let e = lua.load("local x = ").exec().unwrap_err();
    assert!(e.message.contains("Expected"), "{}", e.message);
    let e = lua
        .load(
            "local function inner() error('boom') end\nlocal function outer() inner() end\nouter()",
        )
        .name("script")
        .exec()
        .unwrap_err();
    assert!(
        e.message.contains("boom") && e.message.contains("script"),
        "{}",
        e.message
    );
    let traceback = e
        .traceback
        .as_deref()
        .expect("runtime errors should carry a traceback");
    assert!(traceback.contains("script"), "{traceback}");
    assert!(
        traceback.contains("inner") && traceback.contains("outer"),
        "{traceback}"
    );
    assert!(e.to_string().contains("script"));
}

#[test]
fn tables() {
    let lua = Luau::new();
    let t = lua.create_table();
    t.set("name", "luau").unwrap();
    t.set(1, 10).unwrap();
    t.set(2, 20).unwrap();
    lua.globals().set("t", t.clone()).unwrap();
    let n: f64 = lua.load("return #t + t[1] + t[2]").eval().unwrap();
    assert_eq!(n, 32.0);
    let name: String = t.get("name").unwrap();
    assert_eq!(name, "luau");
    assert_eq!(t.len(), 2);
    assert_eq!(t.sequence().len(), 2);
    let from_lua: Table = lua.load("return {a = 1, b = {c = 'deep'}}").eval().unwrap();
    let inner: Table = from_lua.get("b").unwrap();
    let c: String = inner.get("c").unwrap();
    assert_eq!(c, "deep");
    assert_eq!(from_lua.pairs().len(), 2);
}

#[test]
fn metamethods_apply() {
    let lua = Luau::new();
    let t: Table = lua
        .load("return setmetatable({}, {__index = function(_, k) return k .. '!' end})")
        .eval()
        .unwrap();
    let v: String = t.get("hi").unwrap();
    assert_eq!(v, "hi!");
    let raw: Option<String> = t.raw_get("hi").unwrap();
    assert_eq!(raw, None);
}

#[test]
fn rust_functions() {
    let lua = Luau::new();
    let calls = Rc::new(RefCell::new(0));
    let c = calls.clone();
    let add = lua.create_function(move |_, args| {
        *c.borrow_mut() += 1;
        let a = args
            .first()
            .and_then(|v| v.as_number())
            .ok_or_else(|| Error::new("expected numbers"))?;
        let b = args
            .get(1)
            .and_then(|v| v.as_number())
            .ok_or_else(|| Error::new("expected numbers"))?;
        Ok(vec![Value::from(a + b)])
    });
    lua.globals().set("add", add).unwrap();
    let r: f64 = lua
        .load("local s = 0 for i = 1, 100 do s = add(s, i) end return s")
        .eval()
        .unwrap();
    assert_eq!(r, 5050.0);
    assert_eq!(*calls.borrow(), 100);
    let e = lua.load("add(1)").exec().unwrap_err();
    assert!(e.message.contains("expected numbers"), "{}", e.message);
    let ok: bool = lua.load("return (pcall(add, 1))").eval().unwrap();
    assert!(!ok);
}

#[test]
fn userdata_roundtrip_and_metatable() {
    struct Counter(i64);
    let lua = Luau::new();
    let counter = lua.create_userdata(Counter(40));
    let method_counter = counter.clone();
    let increment = lua.create_function(move |_, args| {
        let amount = args.get(1).and_then(Value::as_number).unwrap_or(1.0) as i64;
        let mut counter = method_counter.borrow_mut()?;
        counter.0 += amount;
        Ok(vec![Value::from(counter.0)])
    });
    let methods = lua.create_table();
    methods.raw_set("Increment", increment).unwrap();
    let metatable = lua.create_table();
    metatable.raw_set("__index", methods).unwrap();
    counter.set_metatable(&metatable).unwrap();
    lua.globals().set("counter", counter.clone()).unwrap();

    let value: i64 = lua.load("return counter:Increment(2)").eval().unwrap();
    assert_eq!(value, 42, "userdata method result");
    let from_luau: UserData<Counter> = lua.load("return counter").eval().unwrap();
    assert_eq!(from_luau.borrow().unwrap().0, 42);
    from_luau.borrow_mut().unwrap().0 += 1;
    assert_eq!(counter.borrow().unwrap().0, 43);
    let wrong_type = lua
        .load("return {} ")
        .eval::<UserData<Counter>>()
        .unwrap_err();
    assert!(wrong_type.message.contains("convert"));

    let foreign_userdata = lua
        .load("return newproxy(true)")
        .eval::<UserData<Counter>>()
        .unwrap_err();
    assert!(foreign_userdata.message.contains("userdata"));
}

#[test]
fn userdata_is_dropped_by_luau_gc() {
    let lua = Luau::new();
    let dropped = Rc::new(RefCell::new(false));
    struct DropFlag(Rc<RefCell<bool>>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            *self.0.borrow_mut() = true;
        }
    }
    let userdata = lua.create_userdata(DropFlag(dropped.clone()));
    lua.globals().set("drop_me", userdata.clone()).unwrap();
    lua.globals().set("drop_me", Value::Nil).unwrap();
    drop(userdata);
    lua.gc_collect();
    assert!(
        *dropped.borrow(),
        "the Rust value should be dropped with its Luau userdata"
    );
}

#[test]
fn state_aware_collection_conversions_and_typed_table_helpers() {
    let lua = Luau::new();
    lua.globals().set("array", vec![1i64, 2, 3]).unwrap();
    let values: Vec<i64> = lua.load("return {10, 20, 30}").eval().unwrap();
    assert_eq!(values, [10, 20, 30]);

    let mut source = HashMap::new();
    source.insert("answer".to_string(), 42i64);
    source.insert("other".to_string(), 7i64);
    lua.set_global("mapping", source).unwrap();
    let mapping: HashMap<String, i64> = lua.load("return {answer = 42, other = 7}").eval().unwrap();
    assert_eq!(mapping["answer"], 42);
    assert_eq!(mapping["other"], 7);

    let table: Table = lua
        .load("return setmetatable({x = 5, [1] = 'a', [2] = 'b'}, {__index = {fallback = 9}})")
        .eval()
        .unwrap();
    let x: i64 = table.get_or("x", 0i64).unwrap();
    let fallback: i64 = table.get_or("fallback", 0i64).unwrap();
    let missing: i64 = table.get_or("missing", 11i64).unwrap();
    assert_eq!((x, fallback, missing), (5, 9, 11));
    let seq: Vec<String> = table.sequence_typed().unwrap();
    assert_eq!(seq, ["a", "b"]);
    let pairs_table: Table = lua.load("return {x = 1, y = 2}").eval().unwrap();
    let pairs: Vec<(String, i64)> = pairs_table.pairs_typed().unwrap();
    assert!(pairs.iter().any(|(key, value)| key == "x" && *value == 1));
}

#[test]
fn call_luau_functions() {
    let lua = Luau::new();
    let f: Function = lua
        .load("return function(a, b) return a .. b, #a end")
        .eval()
        .unwrap();
    let r = f.call(("foo", "bar")).unwrap();
    assert_eq!(r[0].as_str(), Some("foobar"));
    let joined: String = f.call_get(("abcd", "")).unwrap();
    assert_eq!(joined, "abcd");
    assert_eq!(f.call(("abcd", "")).unwrap()[1].as_number(), Some(4.0));
}

#[test]
fn bytecode_roundtrip() {
    let lua = Luau::new();
    let bc = lua.compile("return 7 * 6").unwrap();
    let r: f64 = lua.load(&bc).bytecode().eval().unwrap();
    assert_eq!(r, 42.0);
    assert!(lua.compile("return +").is_err());
}

#[test]
fn task_and_signal() {
    let lua = Luau::new();
    lua.load(
        r#"
        order = {}
        local s = Signal.new()
        s:Connect(function(v) table.insert(order, "fired " .. v) end)
        task.defer(function() table.insert(order, "deferred") end)
        task.delay(0.01, function() s:Fire("late") end)
        table.insert(order, "main")
        local got = s:Wait()
        table.insert(order, "waited " .. got)
        "#,
    )
    .exec()
    .unwrap();
    let order: Table = lua.get_global("order").unwrap();
    let got: Vec<String> = order
        .sequence()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(got, ["main", "deferred", "fired late", "waited late"]);
}

#[test]
fn sandboxed_state() {
    let lua = Luau::with_options(Options {
        sandbox: true,
        ..Options::default()
    });
    let e = lua.load("string.len = nil").exec().unwrap_err();
    assert!(e.message.contains("readonly"), "{}", e.message);
    lua.load("myglobal = 1").exec().unwrap();
}

#[test]
fn interpreter_only() {
    let lua = Luau::with_options(Options {
        codegen: false,
        ..Options::default()
    });
    let r: f64 = lua
        .load("local s = 0 for i = 1, 1000 do s += i end return s")
        .eval()
        .unwrap();
    assert_eq!(r, 500500.0);
}
