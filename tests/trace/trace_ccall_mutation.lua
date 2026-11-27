-- C functions called by value (not as builtins) that change tables
local lib = { rs = rawset, sm = setmetatable, ti = table.insert, fr = table.freeze }
local function a(n) local t = {1, 2, 3} local s = 0 for i = 1, n do s = s + t[1] lib.rs(t, 1, t[1] + 1) end return s, t[1] end
print(a(3000))
local function b(n) local t = {5} local s = 0 for i = 1, n do s = s + (t[1] or 0) if i == 2000 then lib.sm(t, { __index = function() return 100 end }) lib.rs(t, 1, nil) end end return s end
print(b(3000))
local function c(n) local t = {} local s = 0 for i = 1, n do lib.ti(t, i) s = s + #t + (t[1] or 0) end return s end
print(c(3000))
local function d(n) local t = { x = 1 } local s = 0 for i = 1, n do s = s + t.x lib.rs(t, "x", i) end return s end
print(d(3000))
local function e(n) local t = {1} local s = 0 local ok = true for i = 1, n do if i == 1500 then lib.fr(t) end ok = pcall(function() t[1] = i end) and ok s = s + t[1] end return s, ok end
print(e(3000))
local function f(n) local t = {3, 1, 2} local s = 0 for i = 1, n do t[1] = (i * 7) % 5 table.sort(t) s = s + t[1] * 100 + t[3] end return s end
print(f(3000))
local function g(n) local t, u = {1, 2, 3}, {9, 9, 9} local s = 0 for i = 1, n do s = s + t[2] u[2] = i table.move(u, 1, 3, 1, t) end return s end
print(g(3000))
local function h(n) local t = {1, 2} local s = 0 for i = 1, n do s = s + (t[1] or -1) if i % 100 == 0 then table.clear(t) else t[1] = i end end return s end
print(h(3000))
local function k(n) local t = {} local s = 0 for i = 1, n do s = s + #t table.insert(t, 1, i) if #t > 10 then table.remove(t) end s = s + t[1] end return s end
print(k(3000))
local function m(n) local t = {} local s = 0 for i = 1, n do s = s + (getmetatable(t) and 1 or 0) if i == 1000 then setmetatable(t, {}) end end return s end
print(m(3000))
