local P = {}
P.__index = P
P.__lt = function(a, b) return a.v < b.v end
P.__le = function(a, b) return a.v <= b.v end
P.__eq = function(a, b) return a.v == b.v end
local function new(v) return setmetatable({v = v}, P) end
local Q = {__lt = function(a, b) return a.k < b.k end}   -- no __le: a <= b is not (b < a)
local function q(k) return setmetatable({k = k}, Q) end
local Weird = {__eq = function(a, b) return false end, __lt = function(a, b) if a.n > 5 then return nil end return 1 end}

local items = {} for i = 1, 300 do items[i] = new((i * 37) % 101) end
local function count(n)
  local lt, le, eq, self_eq = 0, 0, 0, 0
  for r = 1, n do
    for i = 2, #items do
      local a, b = items[i - 1], items[i]
      if a < b then lt = lt + 1 end
      if a <= b then le = le + 1 end
      if a == b then eq = eq + 1 end
      if not (a > b) then lt = lt + 1 end
      if a ~= b then eq = eq + 2 end
      if a == a then self_eq = self_eq + 1 end
    end
  end
  return lt, le, eq, self_eq
end
print(count(5))
local qs = {} for i = 1, 200 do qs[i] = q(i % 13) end
local function qcount(n) local c = 0 for r = 1, n do for i = 2, #qs do if qs[i-1] <= qs[i] then c = c + 1 end if qs[i] >= qs[i-1] then c = c + 10 end end end return c end
print(qcount(5))
local w = {} for i = 1, 100 do w[i] = setmetatable({n = i % 10}, Weird) end
local function wcount(n) local c = 0 for r = 1, n do for i = 1, #w do local x = w[i] if x == x then c = c + 1 end if x < w[1] then c = c + 2 end end end return c end
print(wcount(5))
-- plain tables (no metatable): identity
local plain = {} for i = 1, 100 do plain[i] = (i % 3 == 0) and plain[1] or {} end
local function pcount(n) local c = 0 for r = 1, n do for i = 1, #plain do if plain[i] == plain[1] then c = c + 1 end end end return c end
print(pcount(5))
-- metatable without __eq on one side
local M = {} local mixed = {} for i = 1, 100 do mixed[i] = (i % 2 == 0) and setmetatable({}, M) or setmetatable({v = i}, P) end
local function mcount(n) local c = 0 for r = 1, n do for i = 2, #mixed do if mixed[i] == mixed[i - 1] then c = c + 1 end if mixed[i] == mixed[i] then c = c + 2 end end end return c end
print(mcount(5))
-- metatable changes while running
local function flip(n) local c = 0 local a, b = new(1), new(2) for i = 1, n do if i == n // 2 then P.__lt = function(x, y) return x.v > y.v end end if a < b then c = c + 1 end end return c end
print(flip(1000))
local function errs() local t = {} for i = 1, 100 do local ok, e = pcall(function() return items[1] < {} end) t[#t+1] = tostring(ok) end return t[1] .. t[100] end
print(errs())
