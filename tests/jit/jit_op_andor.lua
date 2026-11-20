-- Register/constant and-or, reverse division, floor division, and JumpIfNotLe.
local function logic(a, b)
    local r = a and b
    local s = a or b
    local t = a and 5
    local u = a or 7
    return r, s, t, u
end

local function arith(a, b)
    local v = 2 / a
    local w = a // 2
    if a >= b then
        return v, w, "ge"
    else
        return v, w, "lt"
    end
end

local function show(...)
    local n = select("#", ...)
    local out = {}
    for i = 1, n do
        local v = select(i, ...)
        if type(v) == "number" then
            out[i] = string.format("%.6g", v)
        else
            out[i] = tostring(v)
        end
    end
    print(table.concat(out, "|"))
end

show(logic(6, 3))
show(logic(3, 6))
show(logic(nil, 3))
show(logic(false, 3))
show(logic(2, 6))

show(arith(6, 3))
show(arith(3, 6))
show(arith(8, 2))
