local function inv(n)
  local k, s = 3, 0
  for i = 1, n do
    s = s + k
    if i % 5 == 0 then k = k + 1 end
  end
  return s, k
end
print(inv(20000))
local function field(n)
  local cfg = {step = 2}
  local s = 0
  for i = 1, n do
    s = s + cfg.step
    if i % 7 == 0 then cfg.step = cfg.step + 1 end
  end
  return s
end
print(field(20000))
local function grow(n)
  local t = {1, 2, 3}
  local s = 0
  for i = 1, n do
    s = s + t[1] + #t
    if i % 9 == 0 then t[#t + 1] = i end
  end
  return s, #t
end
print(grow(5000))
local M1 = {__index = function() return 1 end}
local M2 = {__index = function() return 2 end}
local function meta(n)
  local o = setmetatable({}, M1)
  local s = 0
  for i = 1, n do
    s = s + o.x
    if i % 11 == 0 then setmetatable(o, (i % 22 == 0) and M1 or M2) end
  end
  return s
end
print(meta(10000))
local function branchy(n)
  local a, b, s = 0, 0, 0
  for i = 1, n do
    if i % 3 == 0 then a = a + i elseif i % 3 == 1 then b = b - i else s = s + a + b end
  end
  return a, b, s
end
print(branchy(30000))
