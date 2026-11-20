-- Metatables and metamethods change while the loop is hot, invalidating any
-- cached lookup or guard.
local function make(v)
  return setmetatable({v = v}, {
    __index = function(_, k) return #k + v end,
    __newindex = function(t, k, x) rawset(t, k, x * 2) end,
  })
end

local function use(mode, n)
  local acc = 0
  local o = make(mode)
  for i = 1, n do
    o.v = o.v + (i % 3)
    if i % 10 == 0 then
      if mode % 2 == 0 then
        setmetatable(o, {
          __index = function(_, k) return #k end,
          __newindex = function(t, k, x) rawset(t, k, x + 1) end,
        })
      else
        setmetatable(o, {
          __index = function(_, k) return #k + mode end,
          __newindex = function(t, k, x) rawset(t, k, x - 1) end,
        })
      end
    end
    acc += o.x + o.v
  end
  return acc
end
for i = 1, 4 do use(i, 40) end
print(use(1, 20), use(2, 20))

local function replace_index(n)
  local t = setmetatable({}, {__index = function(_, k) return #k end})
  local acc = 0
  for i = 1, n do
    if i % 8 == 0 then
      setmetatable(t, {__index = function(_, k) return #k + i end})
    end
    acc += t.abc
  end
  return acc
end
for _ = 1, 4 do replace_index(40) end
print(replace_index(24))
