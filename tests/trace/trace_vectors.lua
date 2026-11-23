local V = vector.create
local function fmt(v) return string.format("%.4f %.4f %.4f", v.x, v.y, v.z) end
local function integrate(n)
  local p, vel = V(0, 0, 0), V(1, 2, 3)
  local g = V(0, -9.8, 0)
  for i = 1, n do
    vel = vel + g * 0.001
    p = p + vel * 0.001
    if p.Y < -10 then p = p - V(0, p.y, 0) end
  end
  return p, vel
end
local p, vel = integrate(20000)
print(fmt(p), fmt(vel))
local function ops(n)
  local acc = V(0, 0, 0)
  local s = 0
  for i = 1, n do
    local a = V(i, i * 0.5, -i)
    local b = V(1, 2, 4)
    acc = acc + a / b - (2 * a) // 3 + (-b) * (1 / i) + 2 / b
    s = s + a.x * b.z - a.Z
    if a == V(i, i * 0.5, -i) then s = s + 1 end
    if a ~= b then s = s + 1 end
  end
  return acc, s
end
local acc, s = ops(5000)
print(fmt(acc), s)
local function lib(n)
  local s = 0
  local k = vector.create(1, 1, 1)
  for i = 1, n do
    local a = V(i % 7, i % 5, i % 3)
    s = s + vector.magnitude(a) + vector.dot(a, k)
    local c = vector.cross(a, k)
    s = s + c.x + vector.normalize(k).y
    local fl = vector.floor(a * 0.7)
    s = s + fl.x + fl.y + fl.z
  end
  return s
end
print(string.format("%.6f", lib(5000)))
local function store(n)
  local t = {}
  for i = 1, n do t[i] = V(i, -i, i * 2) end
  local sum = V(0, 0, 0)
  for i = 1, n do sum = sum + t[i] end
  local o = {pos = V(1, 2, 3)}
  for i = 1, n do o.pos = o.pos * 1.0001 end
  return sum, o.pos
end
local sum, pos = store(3000)
print(fmt(sum), fmt(pos))
local function many(n)
  local a, b, c, d, e = V(1,0,0), V(0,1,0), V(0,0,1), V(1,1,1), V(2,2,2)
  local f, g, h = V(3,1,2), V(0.5,0.5,0.5), V(-1,-2,-3)
  for i = 1, n do
    a, b, c, d, e, f, g, h = b + c * 0.5, c - a, d + e, e * 0.25, f + g, g + h, h * 0.9, a + b
  end
  return fmt(a) .. " " .. fmt(h)
end
print(many(1000))
local function bad(n) local v = V(1, 2, 3) local ok, err = pcall(function() return v + 1 end) return tostring(ok) end
print(bad(1))
