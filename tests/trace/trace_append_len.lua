-- #t inline (the cached border, moved up by appends) and table.insert(t, v)
local function a(n) local t = {} for i = 1, n do t[#t + 1] = i * 2 end return #t, t[1], t[n] end
print(a(5000))
local function b(n) local t = {} for i = 1, n do table.insert(t, i) end return #t, t[n] end
print(b(5000))
-- removals and refills
local function c(n)
  local t, s = {}, 0
  for i = 1, n do
    t[#t + 1] = i
    if i % 3 == 0 then t[#t] = nil end
    if i % 7 == 0 then table.insert(t, -i) end
    s = s + #t
  end
  return s, #t
end
print(c(6000))
-- holes: any border is fine, but the same one the interpreter finds
local function d(n)
  local t = {}
  for i = 1, 64 do t[i] = i end
  t[10] = nil
  local s = 0
  for i = 1, n do s = s + #t end
  return s
end
print(d(1000))
-- a hash part, keys past the array
local function e(n) local t = { x = 1 } for i = 1, n do t[#t + 1] = i if i % 50 == 0 then t["k" .. i] = i end end return #t end
print(e(3000))
-- table.insert with a position, into tables with metatables, frozen
local function f(n) local t = {} for i = 1, n do table.insert(t, 1, i) end return #t, t[1], t[n] end
print(f(500))
local mt = setmetatable({}, { __newindex = function(t, k, v) rawset(t, k, v * 10) end })
local function g(n) for i = 1, n do table.insert(mt, i) end return #mt, mt[1], mt[n] end
print(g(400))
local fr = table.freeze({ 1, 2, 3 })
print(pcall(function() for i = 1, 300 do if i == 250 then table.insert(fr, i) end end end))
local ok = 0
local function h(n) local t = {} for i = 1, n do table.insert(t, i) if i == n - 10 then t = setmetatable({}, { __len = function() return 99 end }) end ok = ok + #t end return ok end
print(h(2000))
