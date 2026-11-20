-- arithmetic with metamethods from native code: userdata, tables, mixed
local p = newproxy(true)
local m = getmetatable(p)
m.__add = function(a, b) return (type(a) == "number" and a or 100) + (type(b) == "number" and b or 100) end
m.__sub = function(a, b) return "sub" end
m.__mul = function(a, b) return {v = 3} end
m.__div = function(a, b) return nil end
local V = {}
V.__index = V
V.__add = function(a, b) return setmetatable({x = a.x + (type(b) == "table" and b.x or b)}, V) end
local v = setmetatable({x = 1}, V)
local function f(n)
  local out = {}
  for i = 1, n do
    out[#out + 1] = p + i
    out[#out + 1] = i + p
    out[#out + 1] = p - 1
    out[#out + 1] = (p * 2).v
    out[#out + 1] = tostring(p / 2)
    out[#out + 1] = (v + i).x
    out[#out + 1] = (v + v).x
    out[#out + 1] = i * 2 + 0.5
  end
  return table.concat(out, ",")
end
for _ = 1, 5 do f(3) end
print(f(3))
print(pcall(function() local u = newproxy() return u + 1 end))
print(pcall(function() return {} * 2 end))
