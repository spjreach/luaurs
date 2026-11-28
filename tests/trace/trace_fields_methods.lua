-- fields carried in registers across iterations (several per table), a
-- hoisted check that a method isn't the object's own field
local obj = { a = 1, b = 2, c = 3 }
local function f(n) for i = 1, n do obj.a = obj.b + obj.c obj.b = obj.a - 1 end return obj.a, obj.b, obj.c end
print(f(5000))
-- two names for one table: stores through either
local t1 = { x = 1, y = 2 }
local t2 = t1
local function g(n) for i = 1, n do t1.x = t2.y + 1 t2.y = t1.x * 0.5 end return t1.x, t2.y end
print(g(3000))
-- an exit in the middle: memory holds what the iteration stored
local o2 = { p = 0, q = 0 }
local function h(n)
  for i = 1, n do
    o2.p = o2.p + i
    if i == n - 7 then return o2.p, o2.q end
    o2.q = o2.q + o2.p
  end
end
print(h(4000))
print(o2.p, o2.q)
-- methods: the object gains a field of the method's name mid-loop
local P = {} P.__index = P
function P:v() return self.k * 2 end
local p = setmetatable({ k = 3 }, P)
local function m(n)
  local s = 0
  for i = 1, n do
    s = s + p:v()
    if i == n // 2 then rawset(p, "v", function() return 100 end) end
  end
  return s
end
print(m(4000))
local q = setmetatable({ k = 5 }, P)
local function m2(n) local s = 0 for i = 1, n do s = s + q:v() if i == 3000 then P.v = function(self) return -self.k end end end return s end
print(m2(5000))
-- new objects that never escape, and ones that do
local Pt = {} Pt.__index = Pt
function Pt.new(x, y) return setmetatable({ x = x, y = y }, Pt) end
function Pt:sum() return self.x + self.y end
local keep = {}
local function k(n)
  local s = 0
  for i = 1, n do
    local o = Pt.new(i, -i * 0.5)
    s = s + o.x + o:sum()
    if i % 1000 == 0 then keep[#keep + 1] = o end
  end
  return s, #keep, keep[#keep].y, getmetatable(keep[1]) == Pt
end
print(k(10000))
