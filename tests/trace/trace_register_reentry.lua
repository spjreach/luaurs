-- side traces running their loop's body again with the loop-carried values
-- handed over in registers (or spill slots)
local function a(n)
  local x, y, t, s = 0, 1.5, {}, "s"
  local i = 1
  while i <= n do
    if i % 3 == 0 then x = x + i elseif i % 5 == 0 then x = 7 y = y * 0.5 else y = y + 1 end
    if i % 7 == 0 then t = { i } s = "q" .. (i % 4) end
    i = i + 1
  end
  return x, y, t[1], s
end
print(a(5000))
-- a carried value changing type on a side path (no register hand-off)
local function b(n)
  local v, c = 0, 0
  for i = 1, n do
    if i % 11 == 0 then v = "str" elseif i % 13 == 0 then v = 3 else
      if type(v) == "number" then v = v + 1 else v = 1 end
    end
    c = c + (type(v) == "string" and 1 or 0)
  end
  return v, c
end
print(b(10000))
-- many carried values (some spilled)
local function c(n)
  local a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13, a14 = 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14
  for i = 1, n do
    if i % 2 == 0 then a1, a2, a3 = a2 + 1, a3 * 0.5, a1 - 1 else a4, a5 = a5 + a1, a4 - a2 end
    if i % 9 == 0 then a6, a7, a8, a9 = a7, a8, a9, a6 + 1 end
    if i % 17 == 0 then a10, a11, a12, a13, a14 = a14, a10 + 1, a11, a12 * 1.0001, a13 end
    a1 = a1 % 1000 a5 = a5 % 977 a4 = a4 % 991
  end
  return a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13, a14
end
print(c(20000))
-- fannkuch's swap loop: side traces through an inner repeat loop
local function flips(perm)
  local q, n = {}, #perm
  for i = 1, n do q[i] = perm[i] end
  local q1, f = q[1], 0
  while q1 ~= 1 do
    q[q1] = q1
    if q1 >= 4 then
      local i, j = 2, q1 - 1
      repeat q[i], q[j] = q[j], q[i]; i = i + 1; j = j - 1 until i >= j
    end
    local qq = q[q1]
    q1 = qq
    f = f + 1
    if f > 100 then break end
  end
  return f
end
local tot = 0
for r = 1, 3000 do
  local p = { 1, 2, 3, 4, 5, 6, 7, 8, 9 }
  for k = 9, 2, -1 do local j = (r * 7 + k * 13) % k + 1 p[k], p[j] = p[j], p[k] end
  tot = tot + flips(p)
end
print(tot)
