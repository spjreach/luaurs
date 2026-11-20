-- One register changes number -> string -> table -> nil inside a hot loop.
-- The first iterations specialize the loop; later iterations must leave the
-- trace through side exits without losing the accumulated state.
local function churn(n)
  local total = 0
  for i = 1, n do
    local x = i * 2
    if i % 3 == 0 then x = "s" .. i end
    if i % 5 == 0 then x = {i, i + 1} end
    if i % 7 == 0 then x = nil end
    if type(x) == "number" then
      total += x
    elseif type(x) == "string" then
      total += #x
    elseif type(x) == "table" then
      total += x[1] + x[2]
    else
      total -= 1
    end
  end
  return total
end
for _ = 1, 4 do churn(80) end
print(churn(30))

-- An arithmetic site specializes for numbers, then a guarded type change
-- makes it fail inside pcall on every fourth iteration.
local function guarded(n)
  local acc = 0
  for i = 1, n do
    local x = i
    if i % 4 == 0 then x = "bad" end
    local ok = pcall(function() return x + 1 end)
    acc += ok and 1 or 0
  end
  return acc
end
for _ = 1, 4 do guarded(40) end
print(guarded(20))
