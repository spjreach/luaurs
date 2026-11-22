local p = {x = 1, y = 2, vx = 0.5, vy = -0.25, name = "p"}
local function step(o, n)
  for i = 1, n do
    o.x = o.x + o.vx
    o.y = o.y + o.vy
    if o.x > 100 then o.vx = -o.vx end
  end
  return o.x, o.y
end
print(step(p, 100000))
-- shape change mid-run: rehash moves fields
for k = 1, 20 do p["extra" .. k] = k end
print(step(p, 100000))
-- a different object with other layout
local q = {vy = 1, vx = 2, y = 3, x = 4}
print(step(q, 1000))
-- missing field -> interpreter
local r = {x = 1, y = 2, vx = 1}
print(pcall(step, r, 10))
-- globals
G1 = 0
for i = 1, 100000 do G1 = G1 + i end
print(G1)
-- metatable present but fields exist
local mt = setmetatable({x = 1, y = 1, vx = 1, vy = 1}, {__index = function() return 0 end})
print(step(mt, 1000))
-- read-only table
local ro = table.freeze({x = 1, y = 1, vx = 1, vy = 1})
print(pcall(step, ro, 10))
