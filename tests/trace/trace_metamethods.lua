local C = {}
local function cx(r, i) return setmetatable({re = r, im = i}, C) end
C.__add = function(a, b) return cx(a.re + b.re, a.im + b.im) end
C.__mul = function(a, b) return cx(a.re * b.re - a.im * b.im, a.re * b.im + a.im * b.re) end
C.__sub = function(a, b) if type(b) == "number" then return cx(a.re - b, a.im) end return cx(a.re - b.re, a.im - b.im) end
C.__unm = function(a) return cx(-a.re, -a.im) end
local function iter(n)
  local z, c = cx(0, 0), cx(0.25, 0.5)
  local s = 0
  for i = 1, n do
    z = z * z + c
    if z.re * z.re + z.im * z.im > 4 then z = cx(0, 0) end
    s = s + z.re
  end
  return s
end
print(iter(100000))
local function more(n)
  local z, s = cx(1, 1), 0
  for i = 1, n do local w = -z - 0.5; s = s + w.re + w.im end
  return s
end
print(more(100000))
-- a metamethod with a side effect on an existing table: must stay correct
local log = {n = 0}
local L = {}
L.__add = function(a, b) log.n = log.n + 1; return a.v + b end
local function side(n) local o = setmetatable({v = 1}, L) local s = 0 for i = 1, n do s = s + (o + i) end return s, log.n end
print(side(10000))
-- operand types change
local vals = {}
for i = 1, 300 do vals[i] = (i % 3 == 0) and cx(i, 0) or i end
local function mixed(n) local s = 0 for i = 1, n do local v = vals[(i % 300) + 1] local r = v + 1 s = s + (type(r) == "table" and r.re or r) end return s end
print(pcall(mixed, 30000))
