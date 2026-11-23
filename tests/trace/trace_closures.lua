local scale = 3
local function run(n)
  local s = 0
  for i = 1, n do
    local k = i % 5
    local f = function(x) return x * k + scale end   -- value captures + upvalue
    s = s + f(i)
  end
  return s
end
print(run(100000))
local function keep(n)
  local fs = {}
  for i = 1, n do local j = i fs[(i % 10) + 1] = function() return j end end
  local s = 0 for i = 1, 10 do s = s + fs[i]() end
  return s
end
print(keep(10000))
local function nocap(n) local s = 0 for i = 1, n do local f = function(x) return x + 1 end s = s + f(i) end return s end
print(nocap(100000))
local function sorter(n)
  local total = 0
  for r = 1, n do
    local t = {5, 3, 9, 1}
    local desc = r % 2 == 0
    table.sort(t, function(a, b) if desc then return a > b end return a < b end)
    total = total + t[1]
  end
  return total
end
print(sorter(2000))
local function byref(n) local s = 0 for i = 1, n do local acc = i local g = function() acc = acc + 1 return acc end s = s + g() end return s end
print(byref(10000))
