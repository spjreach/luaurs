-- Nested loops with loop-carried values, continue, break and mixed loop forms.
local function nest(n, m)
  local acc = 0
  for i = 1, n do
    local row = 0
    for j = 1, m do
      if (i + j) % 5 == 0 then continue end
      row += i * j
      if row > 100 then break end
    end
    acc += row
  end
  return acc
end
for _ = 1, 4 do nest(20, 15) end
print(nest(10, 8))

local function while_nest(n)
  local i, sum = 0, 0
  while i < n do
    i += 1
    local j = 0
    repeat
      j += 1
      sum += i * j
    until j >= (i % 7) + 1
  end
  return sum
end
for _ = 1, 4 do while_nest(30) end
print(while_nest(12))

local function mixed(n)
  local total = 0
  for i = 1, n do
    for j = 1, 3 do
      local x = i * j
      if (i + j) % 2 == 0 then x = x + 0.5 end
      total += x
    end
  end
  return total
end
for _ = 1, 4 do mixed(50) end
print(mixed(25))
