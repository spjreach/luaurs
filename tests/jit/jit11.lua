-- vector.create and assert inline paths: interpreter vs native code must agree
local function mk(a, b, c)
    local v = vector.create(a, b, c)
    local w = vector.create(a, b)
    local k = vector.create(a, 2.5)
    return v, w, k, v + w, v * 2, k.x + k.y + k.z
end
local function chain(n)
    local acc = vector.create(0, 0, 0)
    for i = 1, n do
        local a = vector.create(i, i * 0.5, -i)
        a = vector.create(a.x + 1, a.y, a.z)
        acc = acc + a
    end
    return acc
end
local function asserts(v, w)
    assert(v)
    assert(w, "second")
    local r = assert(v)
    assert(v ~= nil)
    return r
end
local function show(...)
    local t = table.pack(...)
    for i = 1, t.n do t[i] = tostring(t[i]) end
    return table.concat(t, " ")
end
for round = 1, 3 do
    print(show(mk(1, 2, 3)))
    print(show(mk(0.1, -1e30, 1e300)))
    print(show(mk(1 / 0, -0, 0 / 0)))
    print(show(chain(1000)))
    print(show(asserts(1, "x")))
    print(show(asserts(0, true)))
    print(show(asserts({} ~= nil, 0)))
    print(show(pcall(asserts, false, 1)))
    print(show(pcall(asserts, nil, 1)))
    print(show(pcall(asserts, 1, nil)))
    print(show(pcall(asserts, 1, false)))
    print(show(pcall(mk, "1", 2, 3)))
    print(show(pcall(mk, 1, {}, 3)))
    print(show(pcall(mk, 1, 2, "z")))
end
