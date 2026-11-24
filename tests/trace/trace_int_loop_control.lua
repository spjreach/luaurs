local function up(a, b, st) local s, n = 0, 0 for i = a, b, st do s = s + i n = n + 1 end return s, n end
print(up(1, 3000.5, 1), up(1, 3000.9, 2), up(0.5, 3000, 1), up(-3000.5, 2.5, 1))
print(up(3000, 1, -1), up(3000.7, 1.2, -1), up(3000, -3000.5, -3), up(5, 1, 1), up(1, 5, -1))
print(up(1, 2^40, 2^30), up(1, 1e12, 1e11), up(2^30 - 5, 2^30 + 3000, 1), up(-2^31, -2^31 + 2000, 1))
local ok = pcall(function() local c = 0 for i = 1, math.huge do c = c + 1 if c == 5000 then error("x") end end end) print(ok)
local function nan(b) local c = 0 for i = 1, b do c = c + 1 end return c end print(nan(0/0), nan(2500.0000001), nan(-0.0))
local function brk(n) local s = 0 for i = 1, n do s = s + i if i * i > 5000000 then break end end return s end print(brk(10000))
