local s = "hello world, this is a string for byte tests"
local function bytesum(n)
  local acc = 0
  for i = 1, n do acc = acc + string.byte(s, (i % #s) + 1) end
  return acc
end
print(bytesum(100000))
local function subs(n)
  local last
  for i = 1, n do last = string.sub(s, (i % 10) + 1, (i % 10) + 5) end
  return last
end
print(subs(100000))
local function bits(n)
  local h = 0
  for i = 1, n do
    h = bit32.bxor(bit32.band(h * 31 + i, 0xffffffff), bit32.rshift(h, 7))
    h = bit32.lrotate(h, 3)
  end
  return h
end
print(bits(100000))
local function mm(n)
  local lo, hi, t = math.huge, -math.huge, 0
  for i = 1, n do
    local x = math.sin(i) * 100
    lo = math.min(lo, x); hi = math.max(hi, x); t = t + math.cos(x)
  end
  return math.floor(lo), math.floor(hi), math.floor(t)
end
print(mm(100000))
local function types(n)
  local c = 0
  local vals = {1, "a", true, {}, 2.5}
  for i = 1, n do if type(vals[(i % 5) + 1]) == "string" then c = c + 1 end end
  return c
end
print(types(100000))
local b = buffer.create(256)
local function buf(n)
  local acc = 0
  for i = 1, n do
    buffer.writeu8(b, i % 256, i % 251)
    acc = acc + buffer.readu8(b, (i * 7) % 256)
  end
  return acc
end
print(buf(100000))
-- declines: byte of a number arg changes type mid-run
local mixed = {}
for i = 1, 200 do mixed[i] = (i < 150) and "abc" or 12345 end
local function decl(n)
  local c = 0
  for i = 1, n do c = c + (string.byte(tostring(mixed[(i % 200) + 1]), 1) or 0) end
  return c
end
print(decl(10000))
print(pcall(function() local t = 0 for i = 1, 100 do t = t + string.byte(nil) end end))
