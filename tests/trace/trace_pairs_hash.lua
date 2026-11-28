-- pairs over hash parts in traces (keys of each type, order, changes)
local t = {}
for i = 1, 200 do t["k" .. i] = i end
for i = 1, 50 do t[i] = -i end
t[true] = 1000 t[2.5] = 2000 t[-7] = 3000
local function walk(tab)
  local n, s, h = 0, 0, 0
  for k, v in pairs(tab) do
    n = n + 1
    s = s + v
    local kk = type(k) == "string" and #k or (type(k) == "number" and k or 7)
    h = (h * 31 + kk * 3 + v) % 1000003
  end
  return n, s, h
end
for r = 1, 30 do walk(t) end
print(walk(t))
-- values cleared while iterating (allowed), the iteration goes on
local function clear(n)
  local u = {}
  for i = 1, n do u["x" .. i] = i end
  local c = 0
  for k, v in pairs(u) do
    if v % 3 == 0 then u[k] = nil end
    c = c + v
  end
  local left = 0
  for _ in pairs(u) do left = left + 1 end
  return c, left
end
for r = 1, 20 do clear(100) end
print(clear(300))
-- nested, early exit, only a hash part, an empty one
local m = { a = { x = 1, y = 2 }, b = { x = 3 }, c = {} }
local function nested(n)
  local s = 0
  for r = 1, n do
    for _, inner in pairs(m) do
      for k2, v2 in pairs(inner) do s = s + v2 + #k2 end
    end
    for k, v in pairs(t) do if v == 3000 then s = s + 1 break end end
  end
  return s
end
print(nested(300))
-- three loop variables, values of changing types
local mixed = { p = 1, q = "two", r = true, s = 4.5 }
local function types(n)
  local cnt = {}
  for r = 1, n do
    for k, v, extra in pairs(mixed) do
      local ty = type(v)
      cnt[ty] = (cnt[ty] or 0) + 1
      if extra ~= nil then cnt.bad = true end
    end
    if r == n // 2 then mixed.q = 2 end
  end
  return cnt.number, cnt.string, cnt.boolean, cnt.bad
end
print(types(400))
