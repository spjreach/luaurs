-- indexing objects that are not tables: native code calls the VM's generic paths
local store = {}
local p = newproxy(true)
local mt = getmetatable(p)
mt.__index = {10, 20, 30, a = "A"}
mt.__newindex = function(_, k, v) store[k] = v end
local q = newproxy(true)
getmetatable(q).__index = function(_, k) return k end
getmetatable(q).__newindex = store

local function run(o, n)
  local acc = 0
  for i = 1, n do
    acc += o[1] + o[2] + o[3]
    o[1] = i
    o.b = i * 2
  end
  return acc
end
local function runs(o, n)
  local s = ""
  for i = 1, n do
    s = tostring(o.a) .. tostring(o[2])
    o.x = s
  end
  return s
end
print(run(p, 100), run(q, 100), runs(p, 10), runs(q, 10))
print(store[1], store.b, store.x)
local t = setmetatable({}, {__index = function(_, k) return k * 3 end})
print(run(t, 5), rawget(t, 1), t.b)
print(pcall(run, nil, 1))
print(pcall(runs, 5, 1))
