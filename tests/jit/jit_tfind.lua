-- table.find over the value kinds the array search specializes
local t1, t2 = {}, {}
local mt = {__eq = function() return true end}
local p, q = setmetatable({}, mt), setmetatable({}, mt)
local arr = {1, -0.0, 0.0, 2.5, 0/0, 7i, -7i, "a", "b", t1, true, false, vector.create(1, 2, 3), t2, p}
local probes = {1, 0, -0.0, 2.5, 0/0, 7i, -7i, 8i, "a", "b", "c", t1, t2, {}, true, false, vector.create(1, 2, 3), 99, q, p}
local out = {}
for _, v in probes do
  out[#out + 1] = tostring(table.find(arr, v)) .. "/" .. tostring(table.find(arr, v, 3)) .. "/" .. tostring(table.find(arr, v, 20))
end
print(table.concat(out, " "))
local holes = {1, 2, nil, 4}
print(table.find(holes, 4), table.find(holes, 2), table.find({}, 1))
local h = {} h[1] = 5 h[3] = 6 h[2] = 7
print(table.find(h, 6), table.find(h, 7))
print(pcall(table.find, arr, 1, 0))
