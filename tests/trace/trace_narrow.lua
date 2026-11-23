local t = {} for i = 1, 100 do t[i] = i * 3 end
local function sum(a, b, step) local s = 0 for i = a, b, step do local v = t[i] if v then s = s + v end end return s end
print(sum(1, 100, 1), sum(100, 1, -1), sum(1, 100, 7), sum(0.5, 99.5, 1), sum(1.5, 50, 0.5))
local function shifted(n, off) local s = 0 for i = 1, n do s = s + (t[i + off] or -1) end return s end
print(shifted(100, 0), shifted(100, 0.5), shifted(100, -50), shifted(100, 2^31), shifted(100, 2^31 - 60), shifted(100, -2^31))
local function big(n) local s = 0 for i = 2^31 - 5, 2^31 + 5 do s = s + (t[i] or 1) end for i = 2^53 - 6, 2^53 - 1 do s = s + (t[i] or 2) end return s end
print(big(1))
local function prod(n) local s = 0 for i = 1, n do for j = 1, 10 do s = s + (t[i * j] or 0) end end return s end
print(prod(20))
local function fl(n) local s = 0 for i = 1, n do s = s + (t[math.floor(i / 2) + 1] or 0) + (t[i // 3 + 1] or 0) + (t[math.ceil(i * 0.7)] or 0) end return s end
print(fl(150))
local function negz(n) local s = 0 local z = -0.0 for i = 1, n do s = s + (t[i + z] or 0) end return s end
print(negz(100))
local function outside(n, k) local s = 0 for i = 1, n do s = s + (t[k] or 5) k = k + 0.25 end return s end
print(outside(100, 1), outside(100, 2))
local function inf(n) local s = 0 local x = 2^1023 for i = 1, n do local ok, v = pcall(function() return t[x * 4] end) s = s + (v or 1) end return s end
print(inf(100))
