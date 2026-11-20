local function f(N) local x = 0 for i = 1, N do x += i * 2 - i / 2 end return x end
print(f(10))
local function fib(n) if n < 2 then return n end return fib(n - 1) + fib(n - 2) end
print(fib(20))
local t = {} for i = 1, 10 do t[i] = i * i end print(#t, t[5])
local s = 0 for i, v in ipairs(t) do s += v end print(s)
print(pcall(function() local x = nil; return x.y end))
local co = coroutine.wrap(function() for i = 1, 3 do coroutine.yield(i) end end) print(co(), co(), co())
print(string.format("%d %s", 5, "ok"), select("#", 1, 2, 3))
local a = 0 while a < 10 do a += 3 end print(a)
repeat a -= 1 until a < 5 print(a)
print(3 < 4, 4 <= 4, not (5 < 2), 1 == 1)
