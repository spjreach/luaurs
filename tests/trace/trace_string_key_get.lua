-- table reads with a string key known only at run time (inline lookup)
local keys = {}
for i = 1, 300 do keys[i] = "k" .. i end
local t = {}
for i = 1, 300, 2 do t[keys[i]] = i end
local function sum(tab, n)
  local s, miss = 0, 0
  for i = 1, n do
    local v = tab[keys[(i % 300) + 1]]
    if v then s = s + v else miss = miss + 1 end
  end
  return s, miss
end
print(sum(t, 30000))
-- the table grows and rehashes while read
local function grow(n)
  local g, s = {}, 0
  for i = 1, n do
    local k = keys[(i % 300) + 1]
    g[k] = (g[k] or 0) + 1
    s = s + g[k]
  end
  return s
end
print(grow(20000))
-- many keys in few buckets (long chains), keys that are numbers too
local mixed = { [1] = "one", [2.5] = "two and a half", x = "ex", [true] = "yes" }
for i = 1, 100 do mixed["s" .. i] = i end
local function m(n)
  local c = 0
  for i = 1, n do
    local k = (i % 3 == 0) and ("s" .. (i % 100 + 1)) or (i % 3 == 1 and "x" or "nope")
    local v = mixed[k]
    if type(v) == "number" then c = c + v elseif v then c = c + 1000 end
  end
  return c
end
print(m(9000))
-- a metatable's __index for the missing ones
local base = { a = 1, b = 2 }
local obj = setmetatable({ c = 3 }, { __index = base })
local names = { "a", "b", "c", "d" }
local function idx(n)
  local s = 0
  for i = 1, n do local v = obj[names[(i % 4) + 1]] s = s + (v or 10) end
  return s
end
print(idx(8000))
-- empty table, single-node table
local e1, e2 = {}, { only = 5 }
local function em(n) local s = 0 for i = 1, n do s = s + (e1[keys[(i % 300) + 1]] or 1) + (e2[i % 2 == 0 and "only" or "other"] or 2) end return s end
print(em(6000))
