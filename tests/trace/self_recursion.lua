-- calls of a function by itself (native fast path): arguments, results,
-- depth, errors, coroutines, other closures of the same function
local function fib(n) if n < 2 then return n end return fib(n - 1) + fib(n - 2) end
print(fib(20))
local function check(t) if t[2] then return t[1] + check(t[2]) - check(t[3]) end return t[1] end
local function tree(i, d) if d > 0 then return { i, tree(2 * i - 1, d - 1), tree(2 * i, d - 1) } end return { i } end
print(check(tree(1, 12)))
-- fewer / more arguments than parameters
local function f(a, b, c) if a == 0 then return b, c end return f(a - 1) end
print(f(5, 1, 2))
local function g(a, b) if a == 0 then return b end return g(a - 1, b, "extra", "more") end
print(g(10, "b"))
-- several results, results dropped, results kept by a call
local function m(n) if n == 0 then return 1, 2, 3 end local x, y, z = m(n - 1) return z, x, y end
print(m(7))
local function d(n) if n == 0 then return 1, 2, 3 end d(n - 1) return n end
print(d(5))
local function k(n) if n == 0 then return 4, 5 end return n, k(n - 1) end
print(k(3))
-- deep recursion, then too deep
local function depth(n) if n == 0 then return 0 end return 1 + depth(n - 1) end
print(depth(5000))
print(pcall(depth, 1e7))
-- an error deep inside
local function boom(n) if n == 0 then error("bottom") end return boom(n - 1) + 1 end
print(pcall(boom, 300))
-- inside coroutines, yielding from the recursion
local function walk(n) if n == 0 then coroutine.yield("leaf") return 1 end return walk(n - 1) + walk(n - 1) end
local co = coroutine.wrap(function() return walk(4) end)
local leaves = 0
while true do local r = co() if r ~= "leaf" then print(leaves, r) break end leaves = leaves + 1 end
-- other closures of the same function (each calls its own upvalue)
local function make(sc)
  local function h(n) if n == 0 then return sc end return h(n - 1) + sc end
  return h
end
local h1, h2 = make(1), make(10)
print(h1(5), h2(5))
-- a function that calls itself through a variable that changes
local r
local function s(n) if n == 0 then return "s" end return r(n - 1) end
local function t2(n) if n == 0 then return "t" end return s(n - 1) end
r = s print(s(4)) r = t2 print(s(5))
-- recursion through a method
local obj = { n = 0 }
function obj:count(x) if x == 0 then return self.n end self.n = self.n + x return self:count(x - 1) end
print(obj:count(10))
