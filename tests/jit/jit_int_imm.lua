-- integer builtins with constant operands use immediate forms
local function f(a, b)
  local r = {}
  for _ = 1, 50 do
    r = {
      integer.add(a, 5i), integer.sub(a, 5i), integer.mul(a, 3i), integer.mul(a, 0x87c37b91114253d5i),
      integer.band(a, 0xffi), integer.bor(a, 0x100i), integer.bxor(a, -1i), integer.add(a, 0x7fffffffffffi),
      integer.lrotate(a, 31i), integer.rrotate(a, 33i), integer.lrotate(a, 64i), integer.lrotate(a, -1i), integer.rrotate(a, 0i),
      integer.lshift(a, 3i), integer.rshift(a, 3i), integer.lshift(a, -3i), integer.rshift(a, -3i),
      integer.lshift(a, 64i), integer.rshift(a, 70i), integer.lshift(a, 0i), integer.rshift(a, 63i), integer.lshift(a, -64i),
      integer.add(a, b), integer.sub(b, a), integer.mul(a, b), integer.bxor(a, a), integer.sub(a, a),
    }
    a = integer.add(a, 1i)
  end
  return r
end
for _, a in {0i, 1i, -1i, 0x123456789abcdef0i, -0x8000000000000000i, 0x7fffffffffffffffi} do
  local r = f(a, 7i)
  local s = {}
  for i, v in r do s[i] = string.format("%x", v) end
  print(table.concat(s, " "))
end
