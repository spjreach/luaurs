local V = {} V.__index = V
local function v2(x, y) return setmetatable({x = x, y = y}, V) end
V.__add = function(a, b) return v2(a.x + b.x, a.y + b.y) end
V.__mul = function(a, k) return v2(a.x * k, a.y * k) end
local function walk(n, stop)
  local start = v2(1, 2)
  local p = start
  local vel = v2(0.5, -0.25)
  for i = 1, n do
    p = p + vel * 1.0
    if i == stop then break end
  end
  return p.x, p.y, p == start, getmetatable(p) == V
end
print(walk(1000, -1))
print(walk(1000, 1))
print(walk(1000, 2))
print(walk(1000, 600))
local function ident(n)
  local s = 0
  for r = 1, n do
    local c = v2(r, r)
    local z = c
    local k = 0
    repeat
      z = z * 0.5 + c
      k = k + 1
      if k == 1 and r % 7 == 0 then s = s + (rawequal(z, c) and 1000 or 1) end
    until z.x > 3 * r or k > 20
    s = s + k + (rawequal(z, c) and 100000 or 0)
  end
  return s
end
print(ident(300))
local function firstexit(n)
  local s = 0
  for r = 1, n do
    local c = v2(r, -r)
    local z = c
    for k = 1, 10 do
      if r % 11 == 0 and k == 1 then s = s + (z == c and 7 or 3) + z.x end
      z = z + v2(1, 1)
    end
    s = s + z.x - z.y
  end
  return s
end
print(firstexit(500))
local W = {} W.__index = W
local function mt(n)
  local p = v2(0, 0)
  for i = 1, n do
    p = v2(p.x + 1, p.y)
    if i == n // 2 then setmetatable(p, W) end
  end
  return p.x, getmetatable(p) == W
end
print(mt(1000))
local function kept(n)
  local all = {}
  local p = v2(0, 0)
  for i = 1, n do
    p = v2(p.x + 1, p.y + 2)
    if i % 100 == 0 then all[#all + 1] = p end
  end
  return #all, all[3].x, all[3].y, getmetatable(all[3]) == V
end
print(kept(1000))
