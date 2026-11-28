-- function traces: recursion run by traces (calls push frames, returns
-- pop into the caller's continuation)
local function BottomUpTree(item, depth)
  if depth > 0 then
    local i = item + item
    depth = depth - 1
    local left, right = BottomUpTree(i - 1, depth), BottomUpTree(i, depth)
    return { item, left, right }
  else
    return { item }
  end
end
local function ItemCheck(tree)
  if tree[2] then
    return tree[1] + ItemCheck(tree[2]) - ItemCheck(tree[3])
  else
    return tree[1]
  end
end
local total = 0
for d = 1, 12 do
  for k = 1, 30 do total = total + ItemCheck(BottomUpTree(k, d)) end
end
print(total)
-- types changing inside the recursion
local function sum(t, i)
  if i > #t then return 0 end
  local v = t[i]
  if type(v) == "string" then v = #v end
  return v + sum(t, i + 1)
end
local mixed = {}
for i = 1, 300 do mixed[i] = (i % 7 == 0) and ("x"):rep(i % 5) or i * 0.5 end
for r = 1, 50 do total = total + sum(mixed, 1) end
print(total)
-- the function variable reassigned during the recursion
local f
local calls = 0
local function g(n) calls = calls + 1 if n <= 0 then return 0 end return 1 + f(n - 1) end
local function h(n) calls = calls + 1 if n <= 0 then return 100 end if n == 50 then f = g end return 2 + f(n - 1) end
for r = 1, 40 do f = h local x = h(200) total = total + x end
print(total, calls)
-- deep: the stack and CallInfo array grow while traced
local function deep(n) if n == 0 then return 0 end return deep(n - 1) + 1 end
for r = 1, 5 do total = total + deep(2000 * r) end
print(total)
print(pcall(deep, 1e6))
-- errors thrown from inside, caught outside; then it runs on
local function boom(n, at) if n == at then error("at " .. n) end if n == 0 then return 0 end return boom(n - 1, at) + 1 end
for r = 1, 60 do local ok, e = pcall(boom, 100, r % 3 == 0 and 50 or -1) if not ok then total = total + 1 else total = total + e end end
print(total)
-- yields from inside the recursion
local function walk(n) if n == 0 then coroutine.yield(1) return 1 end return walk(n - 1) + walk(n - 1) end
for r = 1, 20 do
  local co = coroutine.wrap(function() return walk(6) end)
  local leaves, res = 0, nil
  while true do local v = co() if v == 1 and leaves < 64 then leaves = leaves + 1 else res = v break end end
  total = total + leaves + res
end
print(total)
-- mutual recursion and several results
local even, odd
function even(n) if n == 0 then return true, 0 end local b, c = odd(n - 1) return b, c + 1 end
function odd(n) if n == 0 then return false, 0 end local b, c = even(n - 1) return b, c + 1 end
for r = 1, 100 do local b, c = even(r) total = total + c + (b and 1 or 0) end
print(total)
local function fib(n) if n < 2 then return n end return fib(n - 1) + fib(n - 2) end
print(fib(24))
-- fewer arguments than parameters, upvalues of the recursion
local acc = 0
local function count(n, step) step = step or 1 acc = acc + step if n <= 0 then return acc end return count(n - 1) end
for r = 1, 50 do total = total + count(100, 2) end
print(total, acc)
