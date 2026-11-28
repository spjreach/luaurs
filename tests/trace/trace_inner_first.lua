-- nested while/repeat loops of varying trip counts (inner loops get their
-- own traces before the outer ones)
local function fannkuch(n)
  local p, q, s, sign, maxflips, sum = {}, {}, {}, 1, 0, 0
  for i = 1, n do p[i] = i; q[i] = i; s[i] = i end
  repeat
    local q1 = p[1]
    if q1 ~= 1 then
      for i = 2, n do q[i] = p[i] end
      local flips = 1
      repeat
        local qq = q[q1]
        if qq == 1 then
          sum = sum + sign * flips
          if flips > maxflips then maxflips = flips end
          break
        end
        q[q1] = q1
        if q1 >= 4 then
          local i, j = 2, q1 - 1
          repeat q[i], q[j] = q[j], q[i]; i = i + 1; j = j - 1; until i >= j
        end
        q1 = qq; flips = flips + 1
      until false
    end
    if sign == 1 then
      p[2], p[1] = p[1], p[2]; sign = -1
    else
      p[2], p[3] = p[3], p[2]; sign = 1
      for i = 3, n do
        local sx = s[i]
        if sx ~= 1 then s[i] = sx - 1; break end
        if i == n then return sum, maxflips end
        s[i] = i
        local t = p[1]; for j = 1, i do p[j] = p[j + 1] end; p[i + 1] = t
      end
    end
  until false
end
for n = 5, 8 do print(n, fannkuch(n)) end
-- a side path that gives the table a metatable: the loop's own checks of
-- it must run again
local log = {}
local function f(n)
  local t = {}
  for i = 1, 64 do t[i] = 0 end
  local i = 1
  while i <= n do
    local k = i % 64 + 1
    t[k] = t[k] + 1
    if i == n - 50 then
      setmetatable(t, { __newindex = function(tt, kk, v) log[#log + 1] = kk rawset(tt, kk, v) end })
      t[k] = nil
    end
    local j = 0
    while j < (i % 3) do j = j + 1 t[k] = (t[k] or 0) + j end
    i = i + 1
  end
  local c = 0
  for x = 1, 64 do c = c + (t[x] or 0) end
  return c, #log
end
print(f(3000))
-- inner loops that only ever run once or twice
local function g(n)
  local s = 0
  for i = 1, n do
    local k = i % 2
    repeat s = s + k k = k - 1 until k < 0
    local m = 0
    while m < (i % 3) do m = m + 1 s = s + m end
  end
  return s
end
print(g(20000))
