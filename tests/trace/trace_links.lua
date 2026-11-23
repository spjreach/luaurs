local function nest(n)
  local s = 0
  for i = 1, n do
    for j = 1, (i % 17) + 3 do s = s + i * j end
    s = s - i
  end
  return s
end
print(nest(20000))
local function alloc(n)
  local keep = {}
  for i = 1, n do
    local t = {}
    for j = 1, 6 do t[j] = i + j end
    keep[(i % 100) + 1] = t
  end
  local s = 0 for i = 1, 100 do s = s + keep[i][6] end
  return s, collectgarbage("count") > 0
end
print(alloc(30000))
local function changing(n)
  local s = 0
  local vals = {}
  for i = 1, n do
    local v = (i > n / 2) and "x" or i
    for j = 1, 5 do
      if type(v) == "number" then s = s + v * j else s = s + #v * j end
    end
  end
  return s
end
print(changing(20000))
local function brk(n)
  local s = 0
  for i = 1, n do
    local j = 0
    while true do
      j = j + 1
      if j * j > i then break end
      s = s + j
    end
  end
  return s
end
print(brk(20000))
local function three(n)
  local s = 0
  for a = 1, n do for b = 1, 4 do for c = 1, 3 do s = s + a * b + c end end end
  return s
end
print(three(5000))
local function heap(n)
  local ra = {} for i = 1, n do ra[i] = (i * 7919) % 1000 end
  local l, ir = math.floor(n / 2) + 1, n
  while true do
    local rra
    if l > 1 then l = l - 1 rra = ra[l] else rra = ra[ir] ra[ir] = ra[1] ir = ir - 1 if ir == 1 then ra[1] = rra break end end
    local i, j = l, l * 2
    while j <= ir do
      if j < ir and ra[j] < ra[j + 1] then j = j + 1 end
      if rra < ra[j] then ra[i] = ra[j] i = j j = j + i else j = ir + 1 end
    end
    ra[i] = rra
  end
  local ok = true for i = 1, n - 1 do if ra[i] > ra[i + 1] then ok = false end end
  return ok, ra[1], ra[n]
end
print(heap(5000))
