local vals = {1, -1, 0, -0.0, 0/0, 1/0, -1/0, 2.5, 7, 3, 21, 1e300}
local function a(n) local x, y = 0, 0 for i = 1, n do local v = vals[i % #vals + 1]
  if v < 2 then x = x + v else y = y - v end
  if v <= 0 then x = x * 0.5 end
  if not (v < 1) then y = y + 1 end
  if v == 7 then x = x + 100 elseif v ~= v then y = y + 0.25 elseif v >= 3 then x = x - 3 else y = y * 1.0001 end
  if v == 0 then x = -x end
end return x, y end
print(a(5000))
local function b(n) local s = 0 for i = 1, n do
  local t
  if i % 5 == 0 then t = i * 2 local u = t + 1 s = s + u else t = -i end
  s = s + t
  if i % 3 ~= 1 then if i % 2 == 0 then s = s + 1 else s = s - 1 end else s = s * 0.999 end
end return s end
print(b(30000))
local function c(n) local s, cnt = 0, 0 for i = 1, n do
  if i % 7 == 0 then s = s + i elseif i % 3 == 0 then s = s - i else s = s + 1 end
  if s > 1000 then cnt = cnt + 1 s = s - 1000 end
end return s, cnt end
print(c(100000))
-- not convertible: calls, strings, break, type changes
local function d(n) local s, str, z = 0, "", 0 for i = 1, n do
  if i % 4 == 0 then s = s + math.abs(-i) else s = s - 1 end
  if i % 9 == 0 then str = "x" .. (i % 10) end
  if i % 11 == 0 then z = "s" else z = 1 end
  if s > 1e9 then break end
end return s, str, z end
print(d(20000))
local function e(n) local s = 0 for i = 1, n do
  local m = i % 6
  if m == 1 then s = s + 1 elseif m == 2 then s = s + 2 elseif m == 3 then s = s + 3 elseif m == 4 then s = s + 4 else s = s - 9 end
end return s end
print(e(60000))
-- a value in an arm read after an exit elsewhere
local t = {} for i = 1, 100 do t[i] = i end
local function f(n) local acc, last = 0, 0 for i = 1, n do
  if i % 2 == 0 then last = i * 3 else last = i end
  acc = acc + (t[i % 120 + 1] or last)
end return acc, last end
print(f(20000))
