local t = {} for i = 1, 1024 do t[i] = i * 1.5 end
local function sum(a, b, st) local s = 0 for i = a, b, st do s = s + (t[(i % 1024) + 1] or -1) + (t[i % 64 + 3] or -2) end return s end
print(sum(1, 30000, 1), sum(30000, 1, -1), sum(-5000, 5000, 3), sum(0.5, 3000.5, 1), sum(1, 3000, 0.5))
print(sum(2^31 - 100, 2^31 + 100, 1), sum(-2^31 - 100, -2^31 + 100, 1), sum(2^30 - 50, 2^30 + 50, 7))
print(sum(1, 100000, 70000), sum(1, 2^33, 2^31), sum(1e9, 1e9 + 3000, 1))
local function neg(n) local s = 0 for i = -n, n do s = s + (t[(i % 8) + 1]) + (t[(i - 3) % 16 + 1]) end return s end
print(neg(5000))
local function nest(n) local s = 0 for i = 1, n do for j = 1, 9 do s = s + t[((i + j) % 32) + 1] end end return s end
print(nest(3000))
