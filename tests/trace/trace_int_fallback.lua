-- integer conversions whose entry checks fail: the loops rerun as floats
local function acc(n, x0) local x = x0 for i = 1, n do x = x + i * 0.5 end return x end
print(acc(3000, 0), acc(3000, 0))

print(acc(80000000, 0), acc(3000, 2^60), acc(3000, 0.1))

local function wide(a, b) local s = 0 for i = a, b do s = s + i if i > a + 100000 then break end end return s end
print(wide(1, 3000), wide(3000000000, 3000200000), wide(-2^40, -2^40 + 5000))
local function modl(n, x0) local x = x0 for i = 1, n do x = (x * 3 + i) % 1000003 end return x end
print(modl(3000, 1), modl(3000, 5000000), modl(3000, 7))
