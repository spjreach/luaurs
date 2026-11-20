-- pairs()/next-style iteration over the hash part (inlined FORGLOOP)
local function sorted_dump(t)
  local keys = {}
  for k, v in pairs(t) do
    keys[#keys + 1] = tostring(k) .. "=" .. tostring(v)
  end
  table.sort(keys)
  return table.concat(keys, ",")
end

-- pure hash, mixed key types
local h = { a = 1, b = 2, c = 3, [true] = "t", [2.5] = "f" }
print(sorted_dump(h))

-- array + hash
local m = { 10, 20, 30, x = "x", y = "y" }
m[100] = "far"
print(sorted_dump(m))

-- holes in the hash part after removal
local r = {}
for i = 1, 50 do r["k" .. i] = i end
for i = 1, 50, 3 do r["k" .. i] = nil end
local sum, n = 0, 0
for k, v in pairs(r) do sum = sum + v; n = n + 1 end
print(n, sum)

-- empty table and table with only array
local e = {}
for k, v in pairs(e) do print("never", k, v) end
local arr = { 1, 2, 3 }
local s2 = 0
for k, v in pairs(arr) do s2 = s2 + k * v end
print(s2)

-- break and nested loops
local count = 0
for k1 in pairs(h) do
  for k2 in pairs(h) do
    count = count + 1
    if count > 20 then break end
  end
end
print(count)

-- key/value used after the loop body modifies values (allowed)
local w = { p = 1, q = 2, r = 3 }
for k, v in pairs(w) do w[k] = v * 10 end
print(sorted_dump(w))

-- hot loop so the function gets compiled
local function total(t)
  local s = 0
  for _, v in pairs(t) do s = s + v end
  return s
end
local big = {}
for i = 1, 1000 do big["key" .. i] = i end
local acc = 0
for _ = 1, 200 do acc = acc + total(big) end
print(acc)
