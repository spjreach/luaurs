local out = {}
local function f(a, b, s)
  local c = a + b
  local d = a - b
  local e = a * b
  local g = a / b
  local h = a * s
  local m = s * a
  local q = a / s
  local r = s / b
  return c, d, e, g, h, m, q, r
end
local v1, v2 = vector.create(1, 2, 3), vector.create(4, -5, 0.5)
for i = 1, 20 do
  local res = table.pack(f(v1 * i, v2, i / 3))
  if i % 5 == 0 then for k = 1, res.n do out[#out+1] = tostring(res[k]) end end
end
for i = 1, 10 do local res = table.pack(f(i, i + 1, 2)) out[#out+1] = table.concat(res, ",") end
out[#out+1] = tostring(pcall(f, v1, 1, 2))
out[#out+1] = tostring(pcall(f, "3", "4", 2))
local mt = setmetatable({}, {__add = function() return 42 end, __sub = function() return 1 end, __mul = function() return 2 end, __div = function() return 3 end})
out[#out+1] = tostring(pcall(f, mt, mt, 5))
print(table.concat(out, "\n"))
