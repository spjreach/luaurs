local function fmt(i) return string.format("%d-%s", i, "x") end
local function label(i) return "n" .. i .. "!" end
local function run(n) local c = 0 for i = 1, n do c = c + #fmt(i) + #label(i) end return c end
print(run(50000))
local function bad(i) if i == 900 then return string.rep("x", "oops") end return string.rep("x", 2) end
local function run2(n) local c = 0 for i = 1, n do c = c + #bad(i) end return c end
print(pcall(run2, 1000))
local function cbad(i) return "v" .. (i == 950 and {} or i) end
local function run3(n) local c = 0 for i = 1, n do c = c + #cbad(i) end return c end
print(pcall(run3, 1000))
local mt = {__index = function() return 0 end}
local function newkeys(n) local t = setmetatable({}, mt) for i = 1, n do t[i] = i; t["k" .. (i % 10)] = i end return #t, t.k3 end
print(newkeys(10000))
local ni = {__newindex = function(t, k, v) rawset(t, k, v * 2) end}
local function newidx(n) local t = setmetatable({}, ni) for i = 1, n do t[i] = i end return t[n] end
print(newidx(1000))
local function frozen(n) local t = table.freeze({1, 2}) for i = 1, n do if i == n then t[3] = 1 end end end
print(pcall(frozen, 1000))
