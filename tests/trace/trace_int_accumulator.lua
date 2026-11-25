local function s1(a, b, x0) local x = x0 for i = a, b do x = x + i end return x end
local function s2(a, b, st, x0) local x = x0 for i = a, b, st do x = x - (i * 3 - 7) end return x end
local function s3(n, x0) local x = x0 for i = 1, n do x = 5 + x end return x end
print(s1(1, 30000, 0), s1(-30000, 30000, 7), s1(1, 30000, -0.0), s1(1, 30000, 0.5), s1(1, 30000, 2^52), s1(1, 30000, 2^50), s1(1, 30000, -2^53), s1(1, 30000, 1/0))
print(s2(1, 30000, 1, 0), s2(30000, 1, -1, 3), s2(-1000000000, 1000000000, 100000, 1), s2(1, 30000, 2, 2^51 - 1))
print(s3(30000, 1), s3(30000, 2^53 - 10), s3(30000, 0/0))
print(s1(1, 1000000000, 0) == 500000000500000000, s1(1000000000 - 20000, 1000000000, 2^51))
-- a side trace that sets x huge, then the body re-entered
local t = {} for i = 1, 50 do t[i] = i end
local function side(n) local x = 0 for i = 1, n do x = x + i if t[i % 64 + 1] == nil and i > 20000 then x = x + 2^53 + 2^52 * 0.5 x = x .. "" x = tonumber(x) end end return x end
print(side(30000))
local function side2(n) local x = 0 for i = 1, n do x = x + i if t[i % 64 + 1] == nil then x = x * 1.5 x = x .. "" x = tonumber(x) end end return x end
print(side2(30000))
-- nested forms
local function n1(n, x0) local x = x0 for i = 1, n do x = (x + i) + 1 end return x end
local function n2(n, x0) local x = x0 for i = 1, n do x = x - i - 2 + i * 4 end return x end
local function n3(n, x0) local x = x0 for i = n, 1, -3 do x = 7 - (i - x) end return x end
local function bad1(n, x0) local x = x0 for i = 1, n do x = x + x - i end return x end
local function bad2(n, x0) local x = x0 for i = 1, 60 do x = 2 * x + i end return x end
local function bad3(n, x0) local x = x0 for i = 1, n do x = -x + i end return x end
print(n1(30000, 0), n1(30000, 2^51 - 5), n2(30000, -3), n3(30000, 1), n3(30000, 0.25))
print(bad1(40, 1), bad2(0, 3), bad3(30001, 5))
-- upvalue accumulators
local function mk() local c = 0 return function(s) c = c + s + 1 return c end, function() return c end end
local inc, get = mk()
local function drive(n) local x = 0 for i = 1, n do x = inc(i) end return x end
print(drive(30000), get())
local inc2, get2 = mk()
local function drive2(n) local x = 0 for i = 1, n do x = inc2(i) if i == 25000 then inc2(0.5) end end return x end
print(drive2(30000), get2())
local co = coroutine.wrap(function() local inc3, get3 = mk() for k = 1, 3 do drive(1) for i = 1, 20000 do inc3(i) end coroutine.yield(get3()) end end)
print(co(), co(), co())
