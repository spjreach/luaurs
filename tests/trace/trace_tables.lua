local function append(n)
  local t = {}
  for i = 1, n do t[#t + 1] = i * 2 end
  return #t, t[n]
end
print(append(100000))
local function ins(n)
  local t = {}
  for i = 1, n do table.insert(t, i) end
  local s = 0
  for i = 1, #t do s = s + t[i] end
  return s
end
print(ins(100000))
local function lists(n)
  local s = 0
  for i = 1, n do local t = {i, i + 1, i + 2}; s = s + t[1] + t[3] end
  return s
end
print(lists(100000))
local function keys(n)
  local t, s = {}, 0
  for i = 1, n do
    t[i + 0.5] = i
    t["k" .. (i % 10)] = i
    s = s + (t[i + 0.5] or 0) + (t.missing or 0) + (t.k3 or 0)
  end
  return s
end
print(keys(20000))
local function holes(n)
  local t = {}
  for i = 1, n do t[i] = i end
  for i = 1, n, 2 do t[i] = nil end
  local c = 0
  for i = 1, n do if t[i] then c = c + 1 end end
  return c
end
print(holes(10000))
local function bad(n)
  local t = {}
  for i = 1, n do t[i] = i; if i == n - 3 then t[nil] = 1 end end
end
print(pcall(bad, 1000))
local function nan(n)
  local t = {}
  for i = 1, n do t[i] = i; if i == n - 3 then t[0/0] = 1 end end
end
print(pcall(nan, 1000))
