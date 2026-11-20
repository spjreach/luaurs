-- Table shape and contents change during the hot loop: array growth, hash
-- stores, raw access and table.insert/remove.
local function mutate(n)
  local t = {1, 2, 3, a = 4}
  local acc = 0
  for i = 1, n do
    t[1] = i
    t[2] = t[1] + 1
    t.a = t.a + i
    acc += t[1] + t[2] + t.a
    if i % 9 == 0 then table.insert(t, i) end
    if i % 11 == 0 then table.remove(t) end
  end
  return acc, #t, t[1], t[#t]
end
for _ = 1, 4 do mutate(50) end
print(mutate(20))

local function resize(n)
  local t = {}
  local acc = 0
  for i = 1, n do
    table.insert(t, i)
    if i % 5 == 0 then
      local v = table.remove(t)
      acc += v or 0
    end
    if i % 7 == 0 then t[#t + 1] = i * 2 end
  end
  return acc, #t, t[1], t[#t]
end
for _ = 1, 4 do resize(50) end
print(resize(20))

local function raws(n)
  local t = {x = 1}
  local acc = 0
  for i = 1, n do
    rawset(t, "x", rawget(t, "x") + i)
    acc += t.x
    if i % 13 == 0 then t.y = i end
  end
  return acc, t.x, t.y
end
for _ = 1, 4 do raws(60) end
print(raws(30))
