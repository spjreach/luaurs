-- table.unpack(t) with a variable number of results
local function sum(...)
  local s = 0
  for i = 1, select("#", ...) do s = s + (select(i, ...) or 0) end
  return s, select("#", ...)
end

local t = { 1, 2, 3, 4, 5 }
print(sum(table.unpack(t)))
print(select("#", table.unpack({})))

-- unpack feeding a table constructor and a return
local function copy(x) return { table.unpack(x) } end
print(#copy(t), copy(t)[5])
local function pass(x) return table.unpack(x) end
print(pass({ "a", "b", "c" }))

-- elements living in the hash part: the fast path must decline
local h = {}
h[1] = 10; h[2] = 20; h[4] = 40
print(select("#", table.unpack(h)))

-- nil holes inside the array part
local holes = { 1, nil, 3 }
print(select("#", table.unpack(holes)))

-- hot loop
local acc = 0
local data = {}
for i = 1, 20 do data[i] = i end
for _ = 1, 2000 do
  local s, n = sum(table.unpack(data))
  acc = acc + s + n
end
print(acc)

-- range form table.unpack(t, 1, n)
local r = { 1, 2, 3, 4, 5, 6 }
print(select("#", table.unpack(r, 1, 3)), table.unpack(r, 1, 3))
print(select("#", table.unpack(r, 1, 0)))
print(select("#", table.unpack(r, 1, -1)))
print(select("#", table.unpack(r, 2, 4)), table.unpack(r, 2, 4))
print(select("#", table.unpack(r, 1, 8)))
local acc2 = 0
for _ = 1, 2000 do acc2 = acc2 + select("#", table.unpack(r, 1, 5)) end
print(acc2)
