-- two names for one table: a store through b must be seen through a
local function f(a, b, n)
  local s = 0
  for i = 1, n do
    s = s + a.x
    b.x = b.x + 1
  end
  return s, a.x
end
local t = {x = 0}
print(f(t, t, 1000))
print(f(t, {x = 0}, 1000))
-- array: invariant index load, store through a variable index
local function g(arr, n)
  local s = 0
  for i = 1, n do
    s = s + arr[1]
    arr[(i % 3) + 1] = arr[(i % 3) + 1] + 1
  end
  return s
end
print(g({1, 2, 3}, 1000))
-- same slot written by an inlined call
local function bump(o) o.v = o.v + 2 end
local function h(o, n)
  local s = 0
  for i = 1, n do
    s = s + o.v
    bump(o)
  end
  return s, o.v
end
print(h({v = 1}, 1000))
-- field and array of the same table
local function k(o, n)
  for i = 1, n do
    o[1] = o[1] + o.w
    o.w = o.w + 1
  end
  return o[1], o.w
end
print(k({5, w = 1}, 1000))
-- exits mid-loop must see stored memory
local function m(o, n)
  for i = 1, n do
    o.c = o.c + 1
    if o.c == 500 then o.flag = true end
  end
  return o.c, o.flag
end
print(m({c = 0, flag = false}, 1000))
-- loads in an inner inlined function of two different tables, same layout
local function sum2(p, q) return p.a + q.a end
local function z(p, q, n)
  local s = 0
  for i = 1, n do p.a = p.a + 1; s = s + sum2(p, q) end
  return s
end
local P = {a = 1}
print(z(P, P, 100), z(P, {a = 5}, 100))
