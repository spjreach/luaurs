local function sum(...)
  local s = 0
  for i = 1, select("#", ...) do s = s + select(i, ...) end
  return s
end
local function first(a, ...) local b, c = ... return a + (b or 0) + (c or 0) end
local function count(...) return select("#", ...) end
local function pack(...) local t = {...} return #t end
local function fwd(...) return first(...) end
local function two(x, y, ...) return x * 2 + y + count(...) end
local function run(n)
  local s = 0
  for i = 1, n do
    s = s + first(i) + first(i, 1) + first(i, 1, 2) + first(i, 1, 2, 3)
    s = s + count() + count(i) + count(i, nil, i) + pack(i, i, i)
    s = s + fwd(i, 2, 3) + two(i, 1) + two(i, 1, 5, 6, 7)
    s = s + sum(i, 1, 2) + select(2, i, 10, 20) + select("#", i, i)
  end
  return s
end
print(run(3000))
-- the loop in the vararg function itself
print(sum(table.unpack((function() local t = {} for i = 1, 300 do t[i] = i end return t end)())))
local function avg(...) local n = select("#", ...) local s = 0 for i = 1, n do local v = select(i, ...) if type(v) == "number" then s = s + v end end return s / n end
local acc = 0
for k = 1, 200 do acc = acc + avg(k, 2, 3, "x", 5) + avg(1, 2) end
print(acc)
local function va_loop(...) local s = 0 for i = 1, 2000 do local a, b = ... s = s + (a or 1) * i + (b or 0) end return s end
print(va_loop(3, 4), va_loop(5), va_loop())
local function strs(n) local s = 0 for i = 1, n do s = s + #string.format("%d %d", i, i) + count(string.byte("abc", 1, 3)) end return s end
print(strs(2000))
local function bad(n) local s = 0 for i = 1, n do local ok = pcall(first, i, "x") if not ok then s = s + 1 end s = s + (i % 7 == 0 and first(i, "1") or 0) end return s end
print(pcall(bad, 100))
