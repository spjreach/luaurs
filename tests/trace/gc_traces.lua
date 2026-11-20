-- Allocation and collection inside hot loops. Outputs depend only on values,
-- never on addresses, and collection is forced at deterministic points.
local function alloc(n)
  local total = 0
  for i = 1, n do
    local t = {i, i + 1, i + 2}
    local s = "v" .. i
    total += t[1] + #s
    if i % 10 == 0 then collectgarbage("collect") end
  end
  return total
end
for _ = 1, 4 do alloc(100) end
print(alloc(50))

local function grow(n)
  local acc = 0
  for i = 1, n do
    local t = {}
    for j = 1, (i % 5) + 1 do
      t[j] = j * 2
    end
    acc += #t + t[1]
    if i % 15 == 0 then collectgarbage("collect") end
  end
  return acc
end
for _ = 1, 4 do grow(80) end
print(grow(40))

local function churn(n)
  local n2 = 0
  for i = 1, n do
    local t = {i}
    local u = {i + 1}
    n2 += #t + #u + t[1] + u[1]
    t = nil
    u = nil
    if i % 25 == 0 then collectgarbage("collect") end
  end
  return n2
end
for _ = 1, 4 do churn(120) end
print(churn(60))
