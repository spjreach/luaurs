-- array indexing with a cached integer index; non-integer and negative keys
local function rw(t, k)
  local a = t[k]
  t[k] = (a or 0) + 1
  local b = t[k]
  return a, b
end
local t = { 10, 20, 30 }
for _ = 1, 3000 do rw(t, 2) end
print(t[2])
local h = { [2.5] = 7, [-1] = 3, [0] = 5 }
print(rw(h, 2.5), rw(h, -1), rw(h, 0), rw(h, 4))
print(h[2.5], h[-1], h[0], h[4])

local function sum(arr, ks)
  local s = 0
  for i = 1, #ks do
    local k = ks[i]
    s += (arr[k] or 0) + (arr[k] or 0)
  end
  return s
end
local arr = { 1, 2, 3, 4, 5, [2.5] = 100, [-3] = 1000 }
for _ = 1, 2000 do sum(arr, { 1, 2, 3 }) end
print(sum(arr, { 1, 2.5, 3, -3, 9 }))
