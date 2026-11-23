local function classify(x)
  if x % 3 == 0 then return x * 2 elseif x % 3 == 1 then return -x end
  return 1
end
local function run(n)
  local s = 0
  for i = 1, n do s = s + classify(i) end
  return s
end
for k = 1, 3 do print(run(100000)) end
local function find(t, v)
  for i = 1, #t do if t[i] == v then return i end end
  return nil
end
local arr = {} for i = 1, 200 do arr[i] = i * 3 end
local function many(n) local s = 0 for i = 1, n do s = s + (find(arr, (i % 50) * 3) or 0) end return s end
print(many(20000))
