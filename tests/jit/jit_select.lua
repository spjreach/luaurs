-- select(i, ...) and select("#", ...) inlined on the frame's varargs
local function sum(...)
  local res = 0
  for i = 1, select("#", ...) do
    res += select(i, ...)
  end
  return res
end
local acc = 0
for _ = 1, 3000 do acc += sum(1, 2, 3, 4, 5) end
print(acc)

local function pick(i, ...) return (select(i, ...)) end
print(pick(1, "a", "b"), pick(2, "a", "b"), pcall(pick, 3, "a", "b"))
print(pcall(pick, 0, "a"), pcall(pick, -1, "a", "b"), pcall(pick, 1.5, "x", "y"))
print(pcall(pick, "#", 1, 2, 3), pcall(pick, "2", "p", "q"), pcall(pick, nil, 1))
local function count(...) return select("#", ...) end
print(count(), count(nil), count(nil, nil, 3))
for _ = 1, 3000 do pick(2, 1, 2, 3) end
print(pick(2, 7, 8, 9))
