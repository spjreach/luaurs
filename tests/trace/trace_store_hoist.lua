-- stores into tables in loops: addresses hoisted, values carried in registers
local function a(n) local t = {0, 0, 0} for i = 1, n do t[2] = t[2] + i t[3] = t[3] - 1 end return t[1], t[2], t[3] end
print(a(30000))
local function b(n) local t = {x = 0, y = 1.5} for i = 1, n do t.x = t.x + i t.y = t.y * 1.0001 end return t.x, t.y end
print(b(30000))
-- storage moves mid-loop (new keys, growth)
local function c(n) local t = {0} local s = 0 for i = 1, n do t[1] = t[1] + 1 if i % 1000 == 0 then t[#t + 1] = i t["k" .. i] = i end s = s + t[1] end return s, #t end
print(c(30000))
local function d(n) local t = {1, 2} for i = 1, n do t[1] = t[2] + i t[2] = t[1] - i if i == 20000 then table.insert(t, 1, 0) end end return t[1], t[2], t[3] end
print(d(30000))
-- two tables that may be the same one
local function e(n, p, q) for i = 1, n do p[1] = p[1] + 1 q[1] = q[1] * 2 % 1000 end return p[1], q[1] end
local s = {1} print(e(30000, s, s)) print(e(30000, {1}, {1}))
-- side traces reading values their parent holds in registers
local function f(n) local x, y, z = 0, 1.5, "a" local t = {0} for i = 1, n do x = x + i y = y + 0.25 if i % 3 == 0 then t[1] = t[1] + x z = (i % 2 == 0) and "e" or "o" elseif i % 5 == 0 then t[1] = t[1] - y end end return x, y, z, t[1] end
print(f(30000))
local function g(n) local acc, tab = 0, {1, 2, 3} for i = 1, n do local k = i % 3 + 1 if tab[k] > 1 then acc = acc + tab[k] tab[k] = tab[k] - 1 else tab[k] = tab[k] + 5 acc = acc - 1 end end return acc, tab[1], tab[2], tab[3] end
print(g(30000))
