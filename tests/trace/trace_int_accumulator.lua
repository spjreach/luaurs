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
