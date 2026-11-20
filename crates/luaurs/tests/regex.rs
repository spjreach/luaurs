use luaurs::{Luau, Options, Value};

#[test]
fn documentation_example() {
    let lua = Luau::new();
    let values: Vec<Value> = lua
        .load(
            r#"
            local input = "[DEBUG] (ID): 99821"
            local label = "[DEBUG] (ID): "

            local escaped = Regex.Escape(label)
            local regex = Regex.new(escaped .. "(\\d+)")
            local match = regex:Match(input)

            assert(match)
            assert(regex:Replace(match, "$1") == "99821")
            assert(match.Captures[0] == match.Text)
            return match.Text, match.Captures[0], match.Captures[1], match.Captures[2]
            "#,
        )
        .call(())
        .unwrap();
    assert_eq!(values[0].as_str(), Some("[DEBUG] (ID): 99821"));
    assert_eq!(values[1].as_str(), Some("[DEBUG] (ID): 99821"));
    assert_eq!(values[2].as_str(), Some("99821"));
    assert_eq!(values[3].as_str(), None);
}

#[test]
fn match_api() {
    let lua = Luau::new();
    lua.load(
        r#"
        local re = Regex.new("(?<word>[A-Za-z]+):(\\d+)")
        assert(re:Test("x word:42"))
        assert(re:IsMatch("x word:42"))
        assert(not re:Test("nope"))

        local m = re:Match("word:42")
        assert(m.Text == "word:42")
        assert(m.Value == "word:42")
        assert(m.Start == 1 and m.Finish == 7)
        assert(m.ByteStart == 0 and m.ByteEnd == 7)
        assert(m:GetCapture(0) == "word:42")
        assert(m:GetCapture(1) == "word")
        assert(m:GetCapture(2) == "42")
        assert(m.Captures[1] == "word" and m.Captures[2] == "42")
        assert(m:GetStart(2) == 6 and m:GetEnd(2) == 7)
        assert(m:GetText() == "word:42")
        assert(tostring(m) == "word:42")

        local all = re:MatchMany("a:1 b:2")
        assert(#all == 2)
        assert(all[1].Text == "a:1" and all[2].Text == "b:2")
        assert(all[2].Captures[1] == "b")
        assert(re:Count("a:1 b:2 c:3") == 3)

        local late = re:Find("xx word:7", 2)
        assert(late and late.Start == 4)
        assert(re:Find("none") == nil)

        assert(re:ReplaceAll("a:1 b:2", "${word}=$2") == "a=1 b=2")
        assert(re:Replace("a:1 b:2", "$2/$1") == "1/a b:2")

        local parts = Regex.new("\\s*,\\s*"):Split("a, b,c")
        assert(#parts == 3 and parts[1] == "a" and parts[2] == "b" and parts[3] == "c")
        assert(Regex.new(","):Split("a,b,c", 2)[2] == "b,c")

        assert(typeof(re) == "Regex")
        assert(typeof(m) == "table")
        assert(re.Pattern == "(?<word>[A-Za-z]+):(\\d+)")
        assert(re.GroupCount == 2)
        assert(re.GroupNames.word == 1)
        assert(re.Flags == "")
        assert(tostring(re) == "Regex((?<word>[A-Za-z]+):(\\d+))")
        "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn flags_and_escaping() {
    let lua = Luau::new();
    lua.load(
        r#"
        assert(Regex.new("abc", "i"):Test("ABC"))
        assert(Regex.new("abc", "i"):Match("ABC").Text == "ABC")
        assert(Regex.new("^b", "m"):Test("a\nb"))
        assert(Regex.new("a.b", "s"):Test("a\nb"))
        assert(not Regex.new("a.b"):Test("a\nb"))
        assert(Regex.new("a b # comment", "x"):Test("ab"))
        assert(Regex.new("\\p{L}+"):Match("123h\u{e9}llo").Text == "h\u{e9}llo")
        assert(Regex.new("[[:alpha:]]+"):Match("123abc").Text == "abc")
        assert(Regex.new("\\bfoo\\b"):Test("a foo b"))
        assert(not Regex.new("\\bfoo\\b"):Test("afoob"))

        local raw = "a+b*c?d.e(f)[g]{h}|i\\j"
        local re = Regex.new("^" .. Regex.Escape(raw) .. "$")
        assert(re:Test(raw))

        local wrong = "[DEBUG] (ID): 99821"
        local label = "[DEBUG] (ID): "
        assert(Regex.new(Regex.Escape(label) .. "(\\d+)"):Replace(wrong, "$1") == "99821")
        "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn function_replacement() {
    let lua = Luau::new();
    lua.load(
        r#"
        local re = Regex.new("[a-z]+")
        local out = re:ReplaceAll("one TWO three", function(m)
            return "<" .. m.Text:upper() .. ">"
        end)
        assert(out == "<ONE> TWO <THREE>", out)

        local first = re:Replace("one TWO three", function(m) return m.Text:upper() end)
        assert(first == "ONE TWO three", first)

        local ok, err = pcall(function()
            return re:Replace("abc", function() return nil end)
        end)
        assert(not ok and tostring(err):find("must return a string"))
        "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn captures_and_replacement_edge_cases() {
    let lua = Luau::new();
    lua.load(
        r#"
        assert(Regex.new("(?P<year>\\d{4})-(?<month>\\d{2})"):ReplaceAll("2024-05", "${month}/${year}") == "05/2024")
        assert(Regex.new("(a)?b"):Match("b").Captures[1] == nil)
        assert(Regex.new("(a)?b"):Replace("b", "[$1]") == "[]")

        local re = Regex.new("a*")
        local all = re:MatchMany("ba")
        assert(#all == 3)
        assert(all[1].Start == 1 and all[1].Finish == 0)
        assert(all[2].Text == "a")
        assert(all[3].Start == 3 and all[3].Finish == 2)

        local words = Regex.new("\\w+")
        local count = 0
        for _, m in ipairs(words:MatchMany("one two three")) do
            count += 1
        end
        assert(count == 3)
        "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn errors_are_raised() {
    let lua = Luau::new();
    let error = lua.load(r#"Regex.new("(")"#).exec().unwrap_err();
    assert!(error.message.contains("invalid regex"), "{}", error.message);

    let bad_flag = lua.load(r#"Regex.new("a", "q")"#).exec().unwrap_err();
    assert!(
        bad_flag.message.contains("invalid regex flag"),
        "{}",
        bad_flag.message
    );

    let backref = lua.load(r#"Regex.new("(a)\\1")"#).exec().unwrap_err();
    assert!(
        backref.message.contains("backreferences are not supported"),
        "{}",
        backref.message
    );

    lua.load(r#"assert(not pcall(function() return Regex.new("[a") end))"#)
        .exec()
        .unwrap();
}

#[test]
fn works_in_a_sandbox() {
    let lua = Luau::with_options(Options {
        sandbox: true,
        ..Options::default()
    });
    let count: f64 = lua
        .load(r#"return Regex.new("a+"):Count("aa a")"#)
        .eval()
        .unwrap();
    assert_eq!(count, 2.0);
}

#[test]
fn binary_input() {
    let lua = Luau::new();
    lua.load(
        r#"
        local input = "x\0y 123"
        local m = Regex.new("\\d+"):Match(input)
        assert(m.Text == "123")
        assert(m.Start == 5)
        "#,
    )
    .exec()
    .unwrap();
}
