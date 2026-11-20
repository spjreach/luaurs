-- Frequent side exits: the type of a hot value changes on every iteration.
local function mixed(n)
  local acc = 0
  for i = 1, n do
    local x
    if i % 2 == 0 then
      x = i * 1.5
    elseif i % 3 == 0 then
      x = "s" .. i
    else
      x = {i}
    end
    if type(x) == "number" then
      acc += x
    elseif type(x) == "string" then
      acc += #x
    else
      acc += x[1]
    end
  end
  return acc
end
for _ = 1, 4 do mixed(100) end
print(mixed(50))

local function branches(n)
  local s = 0
  for i = 1, n do
    local x = i
    if i % 2 == 0 then x = x * 2 end
    if i % 3 == 0 then x = x + 0.5 end
    if i % 5 == 0 then x = x // 2 end
    s += x
  end
  return s
end
for _ = 1, 4 do branches(100) end
print(branches(60))

local function pair(n)
  local s = 0
  for i = 1, n do
    local a = (i % 2 == 0) and i or ("x" .. i)
    local b = (i % 3 == 0) and {i} or (i + 0.5)
    s += (type(a) == "number" and a or #a)
    s += (type(b) == "table" and b[1] or b)
  end
  return s
end
for _ = 1, 4 do pair(100) end
print(pair(40))
