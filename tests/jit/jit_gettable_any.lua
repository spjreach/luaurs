-- t[k] with keys of every type (hash path after the number guard bails)
local function look(t, k, n)
  local r = 0
  for _ = 1, n do r += t[k] end
  return r
end
local keys = { 1.5, "s", vector.create(1, 2, 3), {}, newproxy(), true }
for _, k in keys do
  local t = { [k] = 2 }
  print(typeof(k), look(t, k, 3000))
end
-- missing keys: nil without a metatable, __index with one
local function get(t, k) return t[k] end
for _ = 1, 3000 do get({ x = 1 }, "x") end
print(get({}, "nope"), get({}, 2.5))
local mt = setmetatable({}, { __index = function(_, k) return "idx:" .. tostring(k) end })
print(get(mt, "a"), get(mt, 1.25), get(setmetatable({ a = 9 }, { __index = error }), "a"))
print(pcall(get, nil, "a"), pcall(get, 5, 1))
-- array keys still work after the hash path was chosen
local arr = { 10, 20, 30 }
print(get(arr, 2), get(arr, 4), get(arr, "2"))
