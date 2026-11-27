-- constants that compare equal but differ: -0 / 0, and NaN
local function f(n) local s, z1, z2, nn = 0, 0, 0, 0 for i = 1, n do local a, b = -0.0, 0.0 z1 = 1 / a z2 = 1 / b s = s + (i % 2 == 0 and a or b) nn = 0/0 end return z1, z2, 1 / s, nn ~= nn end
print(f(3000))
local function g(n) local t = {} for i = 1, n do t[i % 4 + 1] = (i % 2 == 0) and -0.0 or 0.0 end return tostring(t[1]), tostring(t[2]), tostring(t[3]), tostring(t[4]) end
print(g(3001))
local function h(n) local c = 0 for i = 1, n do local x = i * 0.0 local y = -i * 0.0 if 1 / x > 0 then c = c + 1 end if 1 / y < 0 then c = c + 10 end end return c end
print(h(3000))
