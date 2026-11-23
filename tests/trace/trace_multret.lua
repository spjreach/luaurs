local function g(x) return x * 2 end
local function two(x) return x, x + 1 end
local function f(a, b) return a + (b or 0) end
local function run(n)
  local s = 0
  for i = 1, n do
    s = s + f(g(i))            -- multret result passed on
    s = s + f(two(i))          -- two results as two args
    s = s + math.floor(i / 3)  -- builtin result
  end
  return s
end
print(run(100000))
local function tail(x) return g(x) end      -- return of a call's results
local function run2(n) local s = 0 for i = 1, n do s = s + tail(i) end return s end
print(run2(100000))
local function r3(x) return math.abs(x) end -- return of a builtin's results
local function run3(n) local s = 0 for i = 1, n do s = s + r3(-i) end return s end
print(run3(100000))
local function run4(n) local s = 0 for i = 1, n do s = s + select("#", two(i)) + math.max(two(i)) end return s end
print(run4(1000))
