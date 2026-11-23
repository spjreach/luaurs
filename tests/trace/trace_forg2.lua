local rows = {} for i = 1, 200 do rows[i] = {i, i * 2, i * 3} end
local function sum_ipairs(n) local s = 0 for r = 1, n do for _, row in ipairs(rows) do for _, v in ipairs(row) do s = s + v end end end return s end
print(sum_ipairs(20))
local function sum_pairs(n) local s = 0 for r = 1, n do for _, row in ipairs(rows) do for k, v in pairs(row) do s = s + v * k end end end return s end
print(sum_pairs(20))
local function sum_plain(n) local s = 0 for r = 1, n do for _, row in rows do for _, v in row do s = s + v end end end return s end
print(sum_plain(20))
local mixed = {} for i = 1, 100 do mixed[i] = (i % 7 == 0) and {i, x = 1} or {i, i} end
local function mix(n) local s = 0 for r = 1, n do for _, t in ipairs(mixed) do for k, v in pairs(t) do s = s + v end end end return s end
print(mix(20))
local ragged = {} for i = 1, 100 do local t = {} for j = 1, (i % 5) do t[j] = j end ragged[i] = t end
local function rag(n) local s = 0 for r = 1, n do for _, t in ipairs(ragged) do for _, v in ipairs(t) do s = s + v end end end return s end
print(rag(20))
