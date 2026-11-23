local words = {"apple", "banana", "cherry", "apple", "date", "fig", "banana"}
local function f(n)
  local eq, ne, lt, le, nk = 0, 0, 0, 0, 0
  for i = 1, n do
    local a, b = words[(i % 7) + 1], words[((i * 3) % 7) + 1]
    if a == b then eq = eq + 1 end
    if a ~= b then ne = ne + 1 end
    if a < b then lt = lt + 1 end
    if a <= b then le = le + 1 end
    if a ~= "apple" then nk = nk + 1 end
  end
  return eq, ne, lt, le, nk
end
print(f(100000))
local t1, t2 = {}, {}
local mt = {__eq = function(a, b) return true end}
local e1, e2 = setmetatable({}, mt), setmetatable({}, mt)
local objs = {t1, t2, t1, e1, e2, true, false, true, print, print, 1, "1"}
local function g(n)
  local c = 0
  for i = 1, n do
    local a, b = objs[(i % 12) + 1], objs[((i * 5) % 12) + 1]
    if a == b then c = c + 1 end
  end
  return c
end
print(g(100000))
