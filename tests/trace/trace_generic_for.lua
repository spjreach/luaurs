local arr = {} for i = 1, 1000 do arr[i] = i * 0.5 end
local function sum_ipairs(t) local s = 0 for _, v in ipairs(t) do s = s + v end return s end
local function sum_pairs(t) local s = 0 for k, v in pairs(t) do s = s + v + (type(k) == "number" and k or 0) end return s end
local function sum_plain(t) local s = 0 for k, v in t do s = s + v end return s end
for r = 1, 3 do print(sum_ipairs(arr), sum_pairs(arr), sum_plain(arr)) end
local mixed = {} for i = 1, 500 do mixed[i] = i end
mixed.x = 10; mixed.y = 20
print(sum_pairs(mixed))
local holes = {} for i = 1, 600 do holes[i] = i end
holes[300] = nil
print(sum_ipairs(holes), sum_pairs(holes))
local kinds = {} for i = 1, 400 do kinds[i] = (i < 300) and i or tostring(i) end
local function count(t) local c = 0 for _, v in ipairs(t) do if type(v) == "number" then c = c + 1 end end return c end
print(count(kinds))
local nested = {} for i = 1, 100 do nested[i] = {i, i + 1} end
local function deep(t) local s = 0 for _, p in ipairs(t) do for _, q in ipairs(p) do s = s + q end end return s end
print(deep(nested))
