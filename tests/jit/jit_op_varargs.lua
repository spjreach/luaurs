-- Varargs: PrepVarargs, packed varargs, forwarding and nested vararg functions.
local function count(...)
    return select("#", ...)
end

local function pack(...)
    return { n = select("#", ...), ... }
end

local function forward(...)
    return count(...), ...
end

local function sum(...)
    local s = 0
    for i = 1, select("#", ...) do
        s += select(i, ...)
    end
    return s
end

local function nested(...)
    local function inner(...)
        return select("#", ...), ...
    end
    return inner(...)
end

print(count())
print(count(1, nil, 3))

local p = pack(1, nil, 3)
print(p.n, p[1], p[2], p[3])

print(forward(1, 2, 3))
print(sum(1, 2, 3, 4))
print(nested("a", nil, "c"))
