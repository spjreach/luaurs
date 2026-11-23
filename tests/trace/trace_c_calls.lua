math.randomseed(42)
local function rnd(n)
  local s = 0
  for i = 1, n do s = s + math.random(1, 100) + math.floor(math.random() * 10) end
  return s
end
print(rnd(100000))
local function fmt(n)
  local last, len = nil, 0
  for i = 1, n do last = string.format("%d:%s:%.2f", i, "x", i / 3); len = len + #last end
  return last, len
end
print(fmt(20000))
local function strs(n)
  local c = 0
  for i = 1, n do
    local s = string.rep("ab", (i % 5) + 1)
    c = c + #string.upper(s) + (string.find(s, "ba", 1, true) or 0)
  end
  return c
end
print(strs(20000))
local parts = {"a", "b", "c", 1, 2}
local function cat(n) local c = 0 for i = 1, n do c = c + #table.concat(parts, ",") end return c end
print(cat(20000))
local function clk(n) local t = 0 for i = 1, n do t = t + (os.clock() >= 0 and 1 or 0) end return t end
print(clk(20000))
local function err(n)
  local s = 0
  for i = 1, n do s = s + #string.rep("x", i == n - 2 and -1 or 2) end
  for i = 1, n do s = s + #string.format("%d", i == n - 2 and "oops" or i) end
  return s
end
print(pcall(err, 1000))
local function deep(n, d)  -- stack growth inside the C call path
  if d > 0 then return deep(n, d - 1) end
  local s = 0
  for i = 1, n do s = s + #string.format("%s%s%s", i, i, i) end
  return s
end
print(deep(5000, 150))
