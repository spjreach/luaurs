local function mk() local c = 0 return function(n) for i = 1, n do c = c + i end return c end end
local f = mk() print(f(3000), f(5000))
-- upvalue changes type mid-loop
local function mk2() local c = 0 return function(n) for i = 1, n do c = c + 1 if i == 2500 then c = tostring(c) .. "" c = tonumber(c) end end return c end end
print(mk2()(4000))
-- closures created in the loop (upvalues close every iteration)
local fs = {} local acc = 0
for i = 1, 3000 do local v = i acc = acc + v local g = function() v = v + 1 return v end fs[i % 7 + 1] = g acc = acc + g() end
print(acc, fs[1](), fs[3]())
-- open upvalue on a live slot, written in the loop and read by a callee
local function outer(n) local x = 0 local function rd() return x end local s = 0 for i = 1, n do x = x + 2 s = s + rd() end return s, x end
print(outer(4000))
-- upvalue holding a table / string (gc stores)
local function mk3() local t = {} return function(n) for i = 1, n do t = {i} end return t[1] end end
print(mk3()(3000))
local function mk4() local s = "" return function(n) for i = 1, n do s = (i % 10 == 0) and "a" or "b" end return s end end
print(mk4()(3001))
-- two closures sharing one upvalue, one in a coroutine
local function pair() local c = 0 return function(n) for i = 1, n do c = c + 1 end return c end, function() return c end end
local inc, get = pair()
local co = coroutine.wrap(function() for k = 1, 3 do inc(2000) coroutine.yield(get()) end end)
print(co(), co(), co(), get())
