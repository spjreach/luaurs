local function f(t, n)
  local a, b, c = 0, 0, 0
  for i = 1, n do
    local v = t[(i % #t) + 1]
    if v > 50 then a = a + v elseif v > 20 then b = b + 1 else c = c - v end
  end
  return a, b, c
end
local t = {}
for i = 1, 97 do t[i] = (i * 37) % 100 end
for k = 1, 5 do print(f(t, 100000)) end
local function g(n)
  local x, s = 1, 0
  while x < n do
    if x % 3 == 0 then s = s + x else s = s - 1 end
    x = x + 1
  end
  return s
end
print(g(100000), g(1000))
