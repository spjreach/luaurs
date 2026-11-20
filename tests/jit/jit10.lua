-- integer library builtins: interpreter vs native code must agree
local I = integer.create
local vals = { I(0), I(1), I(-1), I(2), I(63), I(64), I(-63), I(-64), I(65), I(1234567890123), I(-987654321987),
    integer.lshift(I(1), I(62)), integer.bnot(integer.lshift(I(1), I(62))), integer.lshift(I(1), I(63)) }

local function two(a, b)
    return integer.add(a, b), integer.sub(a, b), integer.mul(a, b), integer.band(a, b), integer.bor(a, b),
        integer.bxor(a, b), integer.lrotate(a, b), integer.rrotate(a, b), integer.lshift(a, b), integer.rshift(a, b)
end
local function one(a)
    return integer.bnot(a), integer.neg(a), integer.bswap(a)
end
local function many(a, b, c, d)
    return integer.band(a, b, c), integer.bor(a, b, c, d), integer.bxor(a, b, c, d), integer.bxor(a), integer.band(a, b, c, d)
end
local function konst(a)
    return integer.add(a, I(7)), integer.lrotate(a, I(13)), integer.rshift(a, I(3)), integer.lshift(a, I(-5)),
        integer.bxor(a, I(255)), integer.mul(a, I(-3))
end
local function nested(a, b)
    local x = integer.bxor(integer.add(a, b), integer.lrotate(a, I(7)))
    x = integer.mul(x, integer.bor(b, I(1)))
    return integer.bxor(x, integer.rshift(x, I(29)))
end
local function mixed(a, b)
    -- wrong operand types take the slow path (and may raise)
    return pcall(integer.add, a, b)
end

local function show(...)
    local t = table.pack(...)
    for i = 1, t.n do t[i] = tostring(t[i]) end
    return table.concat(t, " ")
end

for round = 1, 3 do
    local acc = I(0)
    for _, a in vals do
        print(show(one(a)))
        print(show(konst(a)))
        for _, b in vals do
            print(show(two(a, b)))
            acc = integer.bxor(acc, nested(a, b))
        end
    end
    print(show(many(vals[10], vals[11], vals[12], vals[13])))
    print("acc", tostring(acc))
end
print(show(mixed(I(1), 2)))
print(show(mixed(1, I(2))))
print(show(mixed(I(1), "x")))
print(show(pcall(function() return integer.bnot(5) end)))
print(show(pcall(function() return integer.bxor(I(5), I(3), 1) end)))
