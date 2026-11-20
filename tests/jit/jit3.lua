local out = {}
local vals = {0, 1, -1, 2^31, 2^32 + 5, -2^40, 1.9, -1.9, 0xffffffff, 2^53, 0/0, 1e300, 3}
for _, a in vals do
  out[#out+1] = table.concat({bit32.bnot(a), math.abs(a), math.floor(a), math.ceil(a), math.sqrt(math.abs(a))}, " ")
  for _, b in {0, 1, 5, 31, 32, -1, 33, 2^40 + 3, 1.5} do
    out[#out+1] = table.concat({bit32.band(a, b), bit32.bor(a, b), bit32.bxor(a, b), tostring(pcall(bit32.lshift, a, b)), tostring(pcall(bit32.rshift, a, b)), tostring(pcall(bit32.arshift, a, b)), bit32.lrotate(a, b), bit32.rrotate(a, b), math.min(a, b), math.max(a, b)}, " ")
  end
  out[#out+1] = table.concat({bit32.band(a, 0xff), bit32.lshift(a, 3), bit32.rshift(a, 7), bit32.extract(a, 3, 5), bit32.extract(a, 0, 32), bit32.extract(a, 31), math.max(a, 2), math.min(a, -2)}, " ")
end
print(table.concat(out, "\n"))
local o2 = {}
local function f(...) return select("#", ...), ... end
for _, a in {0, 1, -1, 2^32 + 7, 0xdeadbeef, 1.5} do
  o2[#o2+1] = table.concat({bit32.band(a, 0xff, 0xf0), bit32.bor(a, 1, 2, 4), bit32.bxor(a, a, 5, 9, 17)}, " ")
  o2[#o2+1] = tostring(f(bit32.bxor(a, 3))) .. " " .. tostring(f(math.floor(a))) .. " " .. tostring(f(bit32.band(a, 1, 2)))
end
print(table.concat(o2, "\n"))
