local out = {}
local t = {1, 2, nil, 4, x = "a", y = "b"} t[10] = 10
for i, v in ipairs(t) do out[#out+1] = i .. "=" .. tostring(v) end
local keys = {} for k, v in pairs(t) do keys[#keys+1] = tostring(k) .. ":" .. tostring(v) end table.sort(keys) out[#out+1] = table.concat(keys, ",")
local keys2 = {} for k, v in t do keys2[#keys2+1] = tostring(k) end table.sort(keys2) out[#out+1] = table.concat(keys2, ",")
for k, v in next, {5, 6} do out[#out+1] = k .. "/" .. v end
local function iter(s, c) if c < 3 then return c + 1, c * 2 end end
for a, b in iter, nil, 0 do out[#out+1] = a .. "-" .. b end
local big = table.create(1000, 1) local sum = 0 for _, v in ipairs(big) do sum += v end for _, v in big do sum += v end out[#out+1] = sum
local mt = setmetatable({}, {__iter = function(t) return next, {7, 8} end}) for k, v in mt do out[#out+1] = k .. "~" .. v end
local co = coroutine.wrap(function() for i, v in ipairs({1, 2, 3}) do coroutine.yield(v) end end) out[#out+1] = co() .. co() .. co()
local mixed = {} for i = 1, 5 do mixed[i] = i end mixed[3] = nil local c = 0 for k in pairs(mixed) do c += 1 end out[#out+1] = c
print(table.concat(out, "\n"))
