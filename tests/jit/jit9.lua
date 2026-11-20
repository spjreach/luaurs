local function f(n)
  local x = 1
  x = x + n
  x = {}
  x = 2
  for i = 0, 3 do n = n + i end
  local ok, e = pcall(function() return x + 1 end)
  return n, x
end
for i = 1, 3 do print(f(i)) end
local function g(n)
  local a = n + 1
  local t = {}
  a = setmetatable({}, {__add = function() return 7 end})
  local b = 5
  return a + b
end
print(g(1))
local function h(w)
  local Nb = 4
  local Nr = #w / Nb - 1
  local state = {{},{},{},{}}
  for i = 0, 4*Nb-1 do state[(i % 4) + 1][math.floor(i/4) + 1] = w[i + 1] end
  return #state[1]
end
for i = 1, 300 do local w = table.create(64, 1); if h(w) ~= 4 then print("bad") end end
print("done")
