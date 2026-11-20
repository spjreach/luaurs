-- SETTABLE with keys of any type goes through luaV_settable from native code
local function set(t, k, n)
  for i = 1, n do t[k] = i end
  return t[k]
end
local keys = {1, 2.5, "s", vector.create(1, 2, 3), {}, newproxy(), true}
local t = {}
for _, k in keys do print(typeof(k), set(t, k, 100)) end
local log = {}
local m = setmetatable({}, {__newindex = function(_, k, v) log[#log + 1] = tostring(v) end})
set(m, "x", 3)
print(table.concat(log, ","))
print(pcall(set, table.freeze({}), "a", 1))
print(pcall(set, {}, nil, 1))
print(pcall(set, {}, 0/0, 1))
print(pcall(set, 5, "a", 1))
