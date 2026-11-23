local Point = {}
Point.__index = Point
function Point.new(x, y) return setmetatable({x = x, y = y}, Point) end
function Point:len2() return self.x * self.x + self.y * self.y end
function Point:add(o) self.x = self.x + o.x; self.y = self.y + o.y end
local function run(n)
  local p, q, s = Point.new(1, 2), Point.new(0.5, -0.25), 0
  for i = 1, n do p:add(q); s = s + p:len2() end
  return s
end
print(run(100000))
-- method shadowed by an own field halfway
local function run2(n)
  local p, s = Point.new(1, 1), 0
  for i = 1, n do
    if i == n // 2 then p.len2 = function() return 1000 end end
    s = s + p:len2()
  end
  return s
end
print(run2(10000))
-- string methods (C functions with fast builtins)
local str = "the quick brown fox"
local function run3(n)
  local c = 0
  for i = 1, n do c = c + str:byte((i % #str) + 1) + #str:sub(2, 5) end
  return c
end
print(run3(100000))
-- local alias of a C builtin
local sin, floor = math.sin, math.floor
local function run4(n) local s = 0 for i = 1, n do s = s + floor(sin(i) * 100) end return s end
print(run4(100000))
-- method on an object whose class changes
local A = {} A.__index = A function A:v() return 1 end
local B = {} B.__index = B function B:v() return 2 end
local objs = {setmetatable({}, A), setmetatable({}, B)}
local function run5(n) local s = 0 for i = 1, n do s = s + objs[(i % 10 < 7) and 1 or 2]:v() end return s end
print(run5(100000))
