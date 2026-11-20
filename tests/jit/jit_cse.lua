-- common subexpressions and repeated upvalue reads must see every change
local up = 3
local buf = buffer.create(64)
local function bump() up += 1 end
local function f(a, b, n)
  local out = {}
  for i = 1, n do
    local x = a * 4 + b
    local y = a * 4 + b       -- same as x
    a = a + 1                 -- operand changed: a * 4 differs now
    local z = a * 4 + b
    local u1 = up
    up = up + 1               -- SETUPVAL: the next read differs
    local u2 = up
    bump()                    -- a call changes the upvalue too
    local u3 = up
    local m = x               -- alias of x
    local w = m * 4 + b
    local q = x * 4 + b       -- same as w
    local d1 = b / 0.5
    b = 0 / 0                 -- NaN operand
    local d2 = b / 0.5
    buffer.writei16(buf, (i % 4) * 4, x)
    buffer.writei16(buf, (i % 4) * 4 + 2, z)
    out[#out + 1] = string.format("%g %g %g %d %d %d %g %g %g %s %d %d", x, y, z, u1, u2, u3, w, q, d1, tostring(d2), buffer.readi16(buf, (i % 4) * 4), buffer.readi16(buf, (i % 4) * 4 + 2))
    b = i
  end
  return out
end
for _ = 1, 3 do f(1, 2, 50) end
for _, s in f(5, 7, 6) do print(s) end
-- a value used again after a slow path (non-number) in between
local function g(t, k)
  local r = 0
  for i = 1, 20 do
    local p = i * 3
    local v = t[k]        -- may take a slow path
    r += p + (i * 3) + (type(v) == "number" and v or 0)
  end
  return r
end
print(g({x = 1}, "x"), g(setmetatable({}, {__index = function() return 2 end}), "x"))
