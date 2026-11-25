local function mk(body, m) return loadstring((body:gsub("MOD", tostring(m))))() end
local lcg_src = "return function(n, x0) local x = x0 for i = 1, n do x = (x * 1103 + i * 7 - 5000) % MOD end return x end"
local sq_src = "return function(n, x0) local x = x0 for i = 1, n do x = (x * x + 3) % MOD end return x end"
local neg_src = "return function(n, x0) local x = x0 for i = n, 1, -1 do x = (-x - i * 3) % MOD end return x end"
for _, m in ipairs({1000003, 97, 2147483647, 65536, 1009, 2, 3}) do
  local f = mk(lcg_src, m)
  print(m, f(20000, 1), f(20000, -7), f(20000, -0.0), f(20000, 0.5), f(20000, m + 5), f(20000, -(m - 1)), f(20000, 1/0), f(20000, 0/0))
end
for _, m in ipairs({65521, 67108863, 94906267, 2147483647}) do local f = mk(sq_src, m) print(m, f(20000, 2), f(20000, -3)) end
for _, m in ipairs({1000003, 7}) do local f = mk(neg_src, m) print(m, f(20000, 5), f(20000, -5)) end
-- exits reading x mid-loop, a side trace and body re-entry
local t = {} for i = 1, 50 do t[i] = i end
local function mixed(n) local x, s = 1, 0 for i = 1, n do x = (x + i * 3) % 1000003 if t[i % 60 + 1] == nil then s = s + x .. "" s = tonumber(s) end end return x, s end
local function mixed2(n) local x, s = 1, 0 for i = 1, n do x = (x + i * 3) % 13 if t[i % 60 + 1] == nil then s = s + x .. "" s = tonumber(s) end end return x, s end
print(mixed(30000)) print(mixed2(30000))
local function outside(n) local x, acc = 7, 0 for i = 1, n do acc = acc + x / 2 x = (x + i) % 1000 end return x, acc end
print(outside(30000))
local function brk(n) local x = 3 for i = 1, n do x = (x * 31 + i) % 100003 if x == 4242 then break end end return x end
print(brk(200000))
