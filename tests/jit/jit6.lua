local out = {}
local t1, t2 = {}, {}
local mt = {__eq = function(a, b) return true end}
local e1, e2 = setmetatable({}, mt), setmetatable({}, mt)
local v1, v2 = vector.create(1, 2, 3), vector.create(1, 2, 4)
local vals = {nil, false, true, 0, -0.0, 1, 0/0, "a", "a" .. "", "b", t1, t2, e1, e2, v1, v2, vector.create(1, 2, 3), 1i, print}
for i = 1, #vals + 1 do for j = 1, #vals + 1 do
  local a, b = vals[i], vals[j]
  if a == b then out[#out+1] = "1" else out[#out+1] = "0" end
  if a ~= b then out[#out+1] = "x" end
end end
print(table.concat(out))
