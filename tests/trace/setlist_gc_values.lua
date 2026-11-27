-- table constructors of collectable values while the collector runs
local keep = {}
local function node(i, a, b) return { i, a, b, "s" .. (i % 7), function() return i end } end
local function build(d, i) if d == 0 then return node(i, nil, nil) end return node(i, build(d - 1, i * 2), build(d - 1, i * 2 + 1)) end
local function check(t) if t[2] then return t[1] + check(t[2]) - check(t[3]) + #t[4] + t[5]() else return t[1] + #t[4] + t[5]() end end
local total = 0
for r = 1, 60 do
  local t = build(10, r)
  if r % 10 == 0 then keep[#keep + 1] = t end
  local junk = {} for j = 1, 300 do junk[j] = { j, {j}, "x" .. j } end
  total = total + check(t)
end
collectgarbage()
local s = 0 for _, t in ipairs(keep) do s = s + check(t) end
print(total, s, #keep)
local grow = {} for i = 1, 2000 do grow[i] = { {i}, {i + 1}, tostring(i) } if i % 100 == 0 then collectgarbage() end end
local c = 0 for i = 1, #grow do c = c + grow[i][1][1] + grow[i][2][1] + #grow[i][3] end print(c)
