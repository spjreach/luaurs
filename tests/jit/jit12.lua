-- register tracking, typed loads, bail-outs and recompilation, liveness
local function show(...) print(...) end

-- typed field loads whose type changes later
local function sum(o) local s = 0 for i = 1, 3 do s = s + o.a * 2 + o.b end return s end
show(sum({a = 1, b = 2}), sum({a = 1.5, b = -2}))
show(pcall(sum, {a = "3", b = 1}))
show(pcall(sum, {a = {}, b = 1}))
show(sum(setmetatable({}, {__index = function(_, k) return k == "a" and 4 or 5 end})))
show(sum({a = 7, b = 8}))

-- x/y/z on tables, vectors and both at one site
local function xyz(p) return p.x + p.y * 10 + p.z * 100 end
show(xyz({x = 1, y = 2, z = 3}), xyz(vector.create(1, 2, 3)), xyz({x = 4, y = 5, z = 6}), xyz(vector.create(7, 8, 9)))
local function len2(p) local d = p.x * p.x + p.y * p.y return d end
for i = 1, 4 do show(len2(i % 2 == 0 and vector.create(i, 1, 0) or {x = i, y = 2})) end

-- vector arithmetic kept in registers, then mixed
local function vmath(a, b, k)
  local c = a + b
  local d = c * k - a / 2
  local e = -d
  return e, vector.dot(c, d), vector.magnitude(c), vector.normalize(c + vector.create(1, 0, 0)), vector.cross(a, b)
end
show(vmath(vector.create(1, 2, 3), vector.create(4, 5, 6), 2))
show(vmath(vector.create(0.5, -2, 3), vector.create(4, 0, -6), 0.25))
show(pcall(vmath, 1, 2, 3))
local function arith(a, b) return a + b, a - b, a * b, a / b end
show(arith(1, 2)) show(arith(vector.create(1, 2, 3), vector.create(3, 2, 1))) show(arith(3, 4))
show(pcall(arith, vector.create(1, 2, 3), 2)) show(arith(5, 0.5))
show(vector.min(vector.create(1, 5, 3), vector.create(2, 4, 3)), vector.max(vector.create(1, 5, 3), vector.create(2, 4, 3)))
show(vector.abs(vector.create(-1, 2, -3)), vector.floor(vector.create(1.5, -1.5, 2)), vector.ceil(vector.create(1.5, -1.5, 2)))

-- number operators
local function ops(a, b) return a % b, a // b, a ^ 2, a ^ 0.5, a ^ b, -a, a ^ 3 end
show(ops(7, 3)) show(ops(-7, 3)) show(ops(7.5, -2)) show(ops(0, 0)) show(ops(1e308, 10))
show(math.round(0.5), math.round(-0.5), math.round(0.49999999999999994), math.round(2.5), math.round(-2.5), math.round(1e300))
show(math.min(1, 2), math.max(1, 2), math.min(0 / 0, 1), math.max(1, 0 / 0), math.abs(-0.0), math.floor(-0.5), math.ceil(-0.5))
show(math.sin(1), math.cos(1), math.exp(1), math.log(10), math.atan2(1, 2), math.pow(2, 10), math.fmod(-7, 3), math.sqrt(2))

-- bit32 chains with nested calls (last argument is a call)
local function bits(a, b, n)
  local x = bit32.bor(bit32.lshift(a, n), bit32.rshift(b, 32 - n))
  local y = bit32.bxor(x, bit32.band(a, b), bit32.bnot(b))
  return x, y, bit32.lrotate(y, n), bit32.rrotate(y, 7), bit32.arshift(y, 3), bit32.byteswap(x), bit32.countlz(x), bit32.countrz(x),
    bit32.extract(y, 4, 8), bit32.bor(x, bit32.band(y, 0xff))
end
show(bits(0x12345678, 0x9abcdef0, 5)) show(bits(-1, 2 ^ 32 + 5, 31)) show(bits(1.9, -2.9, 1)) show(bits(0, 0, 0))
show(pcall(bits, 1, 2, 40)) show(bits(3, 4, 2))
local function rot(h, l, n) return bit32.bor(bit32.lshift(h, n), bit32.rshift(l, 32 - n)), bit32.bor(bit32.lshift(l, n), bit32.rshift(h, 32 - n)) end
local h, l = 1, 2 for i = 1, 40 do h, l = rot(h, l, i % 31 + 1) end show(h, l)

-- integer library
local function ints(a, b)
  return integer.add(a, b), integer.sub(a, b), integer.mul(a, b), integer.band(a, b), integer.bor(a, b), integer.bxor(a, b),
    integer.lshift(a, 3i), integer.rshift(a, 3i), integer.lrotate(a, 13i), integer.rrotate(a, 13i), integer.bnot(a), integer.neg(a),
    integer.lt(a, b), integer.le(a, b), integer.gt(a, b), integer.ge(a, b), integer.ult(a, b), integer.uge(a, b), integer.min(a, b),
    integer.max(a, b), integer.tonumber(a), integer.bswap(a)
end
show(ints(5i, 3i)) show(ints(-5i, 3i)) show(ints(0x7fffffffffffffffi, -1i)) show(pcall(ints, 1, 2i))
local function divs(a, b) return integer.div(a, b), integer.rem(a, b), integer.udiv(a, b), integer.urem(a, b) end
show(divs(7i, 2i)) show(divs(-7i, 2i)) show(divs(7i, -2i)) show(pcall(divs, 7i, 0i))
local MIN = integer.sub(integer.neg(0x7fffffffffffffffi), 1i)
show(pcall(divs, MIN, -1i)) show(integer.rem(MIN, -1i)) show(divs(9i, 3i))
show(integer.create(5), integer.create(5.5), integer.create(2 ^ 63), integer.create(-2 ^ 63), integer.create(0 / 0), integer.create(-0.0))
local function gcd(a, b) while b ~= 0i do a, b = b, integer.rem(a, b) end return a end
show(gcd(48i, 18i), gcd(17i, 5i), 3i == 3i, 3i ~= 4i)
local buf = buffer.create(32)
buffer.writeinteger(buf, 0, 0x1122334455667788i) buffer.writeinteger(buf, 8, -2i) buffer.writeu32(buf, 16, 0xdeadbeef) buffer.writef32(buf, 20, 1.5)
show(buffer.readinteger(buf, 0), buffer.readinteger(buf, 8), buffer.readu32(buf, 16), buffer.readi32(buf, 16), buffer.readf32(buf, 20), buffer.readu8(buf, 1), buffer.readi8(buf, 8))
show(pcall(buffer.readinteger, buf, 25)) show(pcall(function() return buffer.readinteger(buf, 25) end)) show(pcall(function() buffer.writeinteger(buf, -1, 1i) end))

-- values that die inside a block, across branches, loops and calls
local function live(n)
  local acc, t = 0, {}
  for i = 1, n do
    local a = i * 2
    local b = a + 1
    if i % 3 == 0 then acc = acc + a else acc = acc - b end
    local c = b * b
    t[i] = c
    if c > 50 then acc = acc + #t end
  end
  return acc, t[1], t[n]
end
show(live(10))
local function early(a, b) local x = a * 2 if b then return x end local y = x + 1 return y, x end
show(early(1, true), early(1, false))

-- a local captured by reference and changed by a callee mid-block
local function cap()
  local n = 1
  local function bump() n = n + 0.5 return 0 end
  local a = n + 1
  local b = n + bump() + n
  n = "s"
  local ok = pcall(function() return n + 1 end)
  return a, b, n, ok
end
show(cap())

-- slow paths that rejoin: metamethods, misses, out-of-range indices
local mt = {__index = function(t, k) return k end, __newindex = function(t, k, v) rawset(t, k, v * 2) end, __lt = function() return true end}
local function mm(t, i) local a = t[i] local b = t.q t[i + 1] = 5 t.r = 6 return a, b, rawget(t, i + 1), rawget(t, "r"), t < t end
show(mm(setmetatable({}, mt), 1)) show(pcall(mm, {10, 20}, 1)) show(mm(setmetatable({}, mt), 2))
local function arr(t, i) local v = t[i] local w = t[i + 1] return v, w end
show(arr({1, 2}, 1), arr({1, 2}, 2), arr({1, 2}, 1.5), arr({1, "x"}, 1), arr({[1.5] = 3}, 1.5))
local function cmp(a, b) return a < b, a <= b, a == b, a ~= b end
show(cmp(1, 2)) show(cmp("a", "b")) show(cmp(2, 2)) show(pcall(cmp, 1, "x")) show(cmp(0 / 0, 0 / 0))

-- upvalues and globals read typed
local K1, K2, K3 = 3, "s", 0x10i
G = 5
local function up(i) return K1 * i + G, K2 .. i, integer.add(K3, integer.create(i)) end
show(up(2)) G = "7" show(pcall(up, 2)) G = 1.5 K1 = 0.5 show(up(4))

-- numeric loops: steps, limits, conversions
local function loops() local s = 0 for i = 10, 1, -3 do s = s + i end for i = 1, 0 do s = s + 100 end for i = 1, 3, 0.5 do s = s + i end for i = "1", 2 do s = s + i end return s end
show(loops())
show(pcall(function() for i = 1, "x" do end end))

-- concatenation converts number operands in place
local function cat(n) local s = "a" .. n .. "b" for i = 0, n - 1 do s = s .. i .. "," .. math.floor(i * 1.5) end return s end
for i = 1, 300 do cat(40) end show(cat(5))
local function nest(a, b) return math.max(a, math.floor(b)), bit32.bor(a, bit32.band(b, 7)), {a, math.abs(b)} end
for i = 1, 2000 do local t = {} t[1] = nest(i, i / 3) end show(nest(5, -7.5))
