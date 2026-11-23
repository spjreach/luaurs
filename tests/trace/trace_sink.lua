local P = {} P.__index = P
local function new(x, y) return setmetatable({x = x, y = y}, P) end
function P.len2(p) return p.x * p.x + p.y * p.y end
local function run(n)
  local s, keep = 0, {}
  for i = 1, n do
    local p = new(i, i + 1)
    local q = new(p.x * 2, p.y)
    q.y = q.y + 0.5
    if i % 97 == 0 then
      keep[#keep + 1] = q
      if getmetatable(q) == P then s = s + 1 end
      s = s + p.x
    end
    s = s + q:len2() + p.x
  end
  return s, #keep, keep[3].x, keep[3].y, getmetatable(keep[3]) == P
end
print(run(3000))
local function inner(i)
  local t = new(i, -i)
  if t.x % 50 == 0 then return t end
  return t.x + t.y
end
local function viainline(n)
  local s, got = 0, 0
  for i = 1, n do
    local r = inner(i)
    if type(r) == "table" then got = got + r.x + r.y + (getmetatable(r) == P and 1000 or 0) else s = s + r end
  end
  return s, got
end
print(viainline(3000))
local function consts(n)
  local s = 0
  for i = 1, n do
    local c = {a = 1, b = 2}
    c.a = c.a + i
    s = s + c.a + c.b
    if i == n - 7 then s = s + #tostring(next(c) ~= nil) end
  end
  return s
end
print(consts(2000))
local function empties(n) local s = 0 for i = 1, n do local e = {} if i % 300 == 0 then e[1] = i s = s + #e end s = s + i end return s end
print(empties(2000))
local function exits(n)
  local s = 0
  for i = 1, n do
    local v = {x = i, y = i * 2}
    local w = {x = v.y, y = v.x}
    if w.x > 3000 then s = s + w.y - v.x end
    s = s + w.x
    if i % 13 == 0 then local saved = w s = s + saved.y * 2 end
  end
  return s
end
print(exits(4000))
LUAEOF=1
