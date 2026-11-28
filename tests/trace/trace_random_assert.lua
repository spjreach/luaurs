-- math.random() inline in traces: same sequence as the library function
math.randomseed(7)
local s, mn, mx = 0, 1, 0
for i = 1, 30000 do
  local r = math.random()
  s = s + r
  if r < mn then mn = r end
  if r > mx then mx = r end
  if i % 5000 == 0 then print(i, string.format("%.17g", r)) end
end
print(string.format("%.17g %.17g %.17g", s, mn, mx))
-- mixed with the library's other forms (they share the state)
local acc = 0
for i = 1, 3000 do
  acc = acc + math.random() + math.random(10) + math.random(3, 5)
end
print(string.format("%.17g", acc))
-- assert: its value, its message, a failing one after the loop got hot
local function f(t, n)
  local c = 0
  for i = 1, n do
    local v = assert(t[(i % 3) + 1], "missing")
    assert(i > 0)
    c = c + v
    if i == n - 5 and t.fail then assert(t.fail == 1, "boom " .. i) end
  end
  return c
end
print(f({1, 2, 3}, 1000))
print(pcall(f, {1, 2, 3, fail = 2}, 1000))
print(pcall(f, {1, false, 3}, 1000))
print(pcall(f, {1, nil, 3}, 1000))
print(pcall(function() for i = 1, 1000 do assert(i < 900, "stop") end end))
print(pcall(function() local x = true for i = 1, 1000 do if i == 950 then x = false end assert(x) end end))
-- branchless selects: NaN and signed zeros on both sides
local vals = {1, -1, 0, -0.0, 0/0, 1e300, -1e300, 2.5, 7, 3}
local function g(n)
  local a, b, c, d, e = 0, 0, 0, 0, 0
  for i = 1, n do
    local x, y = vals[i % #vals + 1], vals[(i * 3) % #vals + 1]
    local p = x if y < x then p = y end a = a + (p == p and p or 0.5)
    local q = x if not (x < y) then q = y end b = b + (q == q and q or 0.25)
    local r = x if x <= y then r = -y end c = c + (r == r and r or 1)
    local s = y if x == y then s = 1 / x end d = d + (s == s and (s > 0 and 1 or s < 0 and -1 or 0) or 2)
    local t = y if x ~= y then t = x end e = e + (t == t and t or 3)
  end
  return a, b, c, d, e
end
print(g(5000))
-- heapsort: a loop whose recorded iteration may be its last
local function heapsort(n, ra)
  local j, i, rra
  local l = math.floor(n / 2) + 1
  local ir = n
  while 1 do
    if l > 1 then l = l - 1 rra = ra[l]
    else
      rra = ra[ir] ra[ir] = ra[1] ir = ir - 1
      if ir == 1 then ra[1] = rra return end
    end
    i = l j = l * 2
    while j <= ir do
      if (j < ir) and (ra[j] < ra[j + 1]) then j = j + 1 end
      if rra < ra[j] then ra[i] = ra[j] i = j j = j + i else j = ir + 1 end
    end
    ra[i] = rra
  end
end
for k = 1, 4 do
  local a = {}
  for i = 1, 3000 do a[i] = math.random() end
  heapsort(3000, a)
  local ok = true
  for i = 1, 2999 do if a[i] > a[i + 1] then ok = false end end
  print(k, ok, string.format("%.17g %.17g", a[1], a[3000]))
end
