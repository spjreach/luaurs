local function run(n) local s = 0 for i = 1, n do local acc = i local g = function() acc = acc + 1 return acc end s = s + g() + acc end return s end
print(run(10000))
local function mk(start) local c = start return function() c = c + 1 return c end end
local function counters(n) local s = 0 for i = 1, n do local f = mk(i) s = s + f() + f() end return s end
print(counters(10000))
local function keep(n)
  local fs = {}
  for i = 1, n do local j = i * 2 fs[(i % 16) + 1] = function() j = j + 1 return j end end
  local s = 0 for k = 1, 16 do s = s + fs[k]() + fs[k]() end
  return s
end
print(keep(5000))
local function outer(n)
  local total = 0
  local bump = function(d) total = total + d end
  for i = 1, n do bump(i) total = total + 1 if i % 100 == 0 then bump(-50) end end
  return total
end
print(outer(10000))
local function shared(n)
  local s = 0
  for i = 1, n do
    local x = i
    local get = function() return x end
    local set = function(v) x = v end
    set(get() * 2)
    s = s + get() + x
  end
  return s
end
print(shared(10000))
local function escape(n)
  local last
  for i = 1, n do
    local v = i
    if i % 1000 == 0 then last = function() return v end end
    v = v + 1
  end
  return last()
end
print(escape(10000))
local function brk(n)
  local f
  for i = 1, n do
    local k = i
    f = function() return k end
    if i == n - 3 then break end
    k = k * 10
  end
  return f()
end
print(brk(5000))
local function deep(n)
  local s = 0
  for i = 1, n do
    local a = i
    local function lvl1() local b = a + 1 local function lvl2() a = a + b return a end return lvl2() end
    s = s + lvl1() + a
  end
  return s
end
print(deep(5000))
