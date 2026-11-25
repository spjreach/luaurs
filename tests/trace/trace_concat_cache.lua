-- concat results cached per site: must match string.format through GCs
local bad = 0
local function run(n, m, gcevery, pre)
  local acc = 0
  for i = 1, n do
    local k = pre .. (i % m)
    if k ~= string.format("%s%d", pre, i % m) then bad = bad + 1 end
    acc = acc + #k
    if gcevery > 0 and i % gcevery == 0 then collectgarbage() end
    if i % 7 == 0 then local junk = {} for j = 1, 20 do junk[j] = tostring(j * i) .. "x" end end
  end
  return acc
end
print(run(20000, 100, 0, "a"), run(20000, 5000, 0, "b"), run(20000, 300, 997, "c"), run(30000, 50000, 1501, "d"))
-- string .. string, number .. string, floats, -0
local function mix(n)
  local t = {}
  for i = 1, n do
    local a = (i % 3 == 0) and -0.0 or (i % 17) * 0.5
    local s = a .. "|" 
    local r = "q" .. s
    t[#t + 1] = r
    if i % 10 == 0 then local g = {} for j = 1, 30 do g[j] = {j} end end
  end
  return table.concat(t, ",", 1, 40), #t
end
print(mix(20000))
-- an operand string freed and its address reused between uses
local function fresh(n)
  local c = 0
  for i = 1, n do
    local p = string.rep("p", 1 + i % 5) .. tostring(i % 11)
    local k = p .. 7
    if k ~= p .. "7" then bad = bad + 1 end
    c = c + #k
    if i % 500 == 0 then collectgarbage() end
  end
  return c
end
print(fresh(20000), bad)
