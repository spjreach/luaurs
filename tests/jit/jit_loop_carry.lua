-- values carried across loop iterations in registers must stay exact
local function sums(n)
  local s, p, v = 0, 1, vector.create(0, 0, 0)
  local inv = 0.5
  for i = 1, n do
    s = s + i * inv
    p = p * 1.0001
    v = v + vector.create(i, -i, 0.5)
  end
  return s, p, v
end
print(sums(1000))
local function early(n, stop)
  local acc = 0
  for i = 1, n do
    acc += i
    if i == stop then break end
  end
  return acc
end
print(early(100, 37), early(10, 50))
local function cont(n)
  local acc = 0
  for i = 1, n do
    if i % 3 == 0 then continue end
    acc += i
  end
  return acc
end
print(cont(100))
local function poly(t)
  local acc = 0
  for i = 1, #t do acc = acc + t[i] end   -- may stop being a number
  return acc
end
for _ = 1, 200 do poly({1, 2, 3}) end
print(poly({1, 2, 3}), pcall(poly, {1, "x", 3}), poly({1.5, 2.5}))
local mt = setmetatable({}, {__add = function(a, b) return 1000 end})
local function mixed(n, start)
  local acc = start
  for i = 1, n do acc = acc + i end
  return acc
end
for _ = 1, 200 do mixed(10, 0) end
print(mixed(10, 0), mixed(3, mt), mixed(4, 0.25))
local function nested(n)
  local total = 0
  for i = 1, n do
    local row = 0
    for j = 1, n do row = row + i * j end
    total = total + row
  end
  return total
end
print(nested(50))
local function withcall(n)
  local acc = 0
  for i = 1, n do acc = acc + math.max(i, 3) + tonumber(tostring(i)) end
  return acc
end
print(withcall(100))
local function err(n)
  local acc = 0
  for i = 1, n do
    acc += i
    if i == 5 then error("stop at " .. acc) end
  end
end
print(pcall(err, 10))
