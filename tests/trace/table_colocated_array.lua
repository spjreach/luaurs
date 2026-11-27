-- small array parts allocated with their table
local t = {1, 2, 3}
for i = 4, 40 do t[i] = i end
print(#t, t[1], t[3], t[40])
local u = {1, 2, 3, 4, 5, 6, 7, 8}
for i = 8, 2, -1 do table.remove(u) end
print(#u, u[1]) table.insert(u, 1, 0) table.insert(u, 9) print(#u, u[1], u[2], u[3])
local c = table.clone({7, 8, 9}) c[4] = 10 print(#c, c[1], c[4])
local f = table.freeze({1, 2}) print(pcall(function() f[1] = 5 end), f[1])
local mt = {1, 2, __index = function(_, k) return k * 2 end}
local s = setmetatable({}, mt) print(s[21], mt[1], mt[2])
local m2 = setmetatable({5, 6}, {__len = function() return 99 end}) print(#m2, m2[2], rawlen(m2))
local h = {1, 2} h.x = "a" h.y = "b" h[3] = 3 print(h[1], h[3], h.x)
local e = {} for i = 1, 5 do e[i] = {i} end print(e[5][1])
local function churn(n) local keep = {} for i = 1, n do local x = {i, i + 1, i + 2} if i % 1000 == 0 then keep[#keep + 1] = x x[10] = i end end return #keep, keep[1][1], keep[1][10] end
collectgarbage() local base = collectgarbage("count")
print(churn(200000))
collectgarbage() collectgarbage() local after = collectgarbage("count")
print("memory back to baseline:", after - base < 64)
local w = setmetatable({}, {__mode = "v"}) w[1] = {1} w[2] = {2, 3} collectgarbage() print(w[1], w[2])
local big = {} for i = 1, 20000 do big[i] = {i % 7, i % 5} end local sum = 0 for i = 1, #big do sum = sum + big[i][1] + big[i][2] end print(sum)
print(table.concat({1, 2, 3, 4}, ","), select("#", table.unpack({1, 2, 3})))
