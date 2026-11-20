-- table.insert(t, v) inlined as a direct call
local function fill(n)
  local t = {}
  for i = 1, n do
    table.insert(t, i * 2)
  end
  return t
end

local t = fill(1000)
print(#t, t[1], t[500], t[1000])

-- constant and string values, nil value appends nothing visible
local s = {}
for i = 1, 10 do
  table.insert(s, "x")
  table.insert(s, i)
end
table.insert(s, nil)
print(#s, s[1], s[2], s[20])

-- table with a hash part and holes
local h = { 1, 2, 3, a = 1 }
h[5] = 5
table.insert(h, 99)
print(h[4], h[5], h[6])

-- frozen table must raise the builtin's error
local f = table.freeze({ 1, 2 })
local ok, err = pcall(function()
  for _ = 1, 3 do
    table.insert(f, 3)
  end
end)
print(ok, err ~= nil)

-- values that need the GC barrier (tables into an old table)
local outer = {}
for i = 1, 200 do
  table.insert(outer, { i })
end
local sum = 0
for i = 1, #outer do sum = sum + outer[i][1] end
print(sum)

-- hot path
local acc = 0
for _ = 1, 100 do
  local q = fill(100)
  acc = acc + #q
end
print(acc)
